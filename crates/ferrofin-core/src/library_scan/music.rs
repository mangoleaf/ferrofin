//! The music pass: the remote providers of music albums and artists
//! (MusicBrainz, TheAudioDB, the plugins' metadata sources, fanart) and the
//! album metadata derived from its tracks, run after the item walk under
//! each item's own refresh decision.
//!
//! Upstream refreshes a `MusicAlbum` — an `IMetadataContainer` — after its
//! tracks (`MusicAlbum.RefreshAllMetadata`), so `AlbumMetadataService` reads
//! probed tracks. The walk plans an album before its tracks, so the album's
//! providers wait for this pass; the DECISION is still the walk's, made from
//! the album's stored row as it stood before the scan
//! (`MetadataService.RefreshMetadata`'s `isFirstRefresh`/`requiresRefresh`
//! and `GetProviders`, `MetadataService.cs:89-270,647-715`). So an unchanged
//! album or artist runs nothing here and makes no request; a new, changed
//! (folder mtime drift, D3), interval-expired, "Search for missing metadata"
//! or "Replace all metadata" one runs its providers once, merged by
//! `RefreshWithProviders`' rule ([`merge_refresh`]).
//!
//! An artist known only by name (a compilation's album artist: no folder, no
//! library) is never walked. Upstream's `ArtistsValidator` refreshes it after
//! each library validation when it is new or was never refreshed
//! (`ArtistsValidator.cs:86-91`), and `POST /Items/{id}/Refresh` or an
//! Identify refreshes it with the request's options
//! ([`refresh_by_name_artist`](LibraryScanner::refresh_by_name_artist)).

use ferrofin_traits::providers::ItemUpdateType;
use std::collections::{HashMap, HashSet};

use chrono::Utc;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::entities::MetadataField;
use ferrofin_model::providers::RemoteSearchResult;
use ferrofin_providers::library_options::fetcher_names;
use ferrofin_providers::metadata_merge::{
    ALL_LOCKABLE_FIELDS, MetadataResult, RefreshAnswers, RefreshMerge, is_valid_provider_id,
    merge_provider_ids, merge_refresh, settle_sort_name,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{InternalItemsQuery, ItemImageInfo};
use ferrofin_traits::providers::{MetadataRefreshMode, MetadataRefreshOptions};
use uuid::Uuid;

use super::{
    FetcherPolicy, LibraryScanner, RefreshReach, RemoteAnswer, RemoteFold, RemoteImage, ScanCancel,
    ScanOutcome, ScanRun, Served, append_fanart, apply_album_child_metadata, images_changed,
    item_values_of, preferred_language, provider_order, search_result_ids, split_pipe,
};
use crate::item_type_lookup;
use crate::refresh_plan::{
    FileFacts, ImageFetch, ItemRefreshPlan, PassOutcome, ProbeKind, RefreshRequest, StoredState,
};

/// Which music item a [`MusicRefresh`] refreshes, and so which children its
/// metadata derives from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MusicKind {
    /// A `MusicAlbum`: its tracks below it (`GetRecursiveChildren(i => i is
    /// Audio)`, `AlbumMetadataService`).
    Album,
    /// A folder-backed `MusicArtist`: the tracks below it.
    Artist,
    /// A `MusicArtist` known only by name: the items tagged with it
    /// (`GetTaggedItems`, `ArtistMetadataService`).
    ByNameArtist,
}

impl MusicKind {
    /// The kind of a planned item the music pass refreshes, if it is one. A
    /// planned artist is folder-backed.
    pub(super) fn of_planned(entity: &BaseItemEntity) -> Option<Self> {
        match item_type_lookup::kind_from_type_name(&entity.type_) {
            Some(BaseItemKind::MusicAlbum) => Some(Self::Album),
            Some(BaseItemKind::MusicArtist) => Some(Self::Artist),
            _ => None,
        }
    }

    /// The item type the fetcher checkboxes are saved under.
    fn type_name(self) -> &'static str {
        match self {
            Self::Album => "MusicAlbum",
            Self::Artist | Self::ByNameArtist => "MusicArtist",
        }
    }
}

/// Whether an item of kind `short` runs its remote metadata providers in
/// this pass rather than in the walk's: an album or an artist, which upstream
/// refreshes after the tracks below it (see the module docs).
pub(super) fn owns_providers(short: &str) -> bool {
    matches!(short, "MusicAlbum" | "MusicArtist")
}

/// One album or artist this scan owes a music refresh, with the decision
/// the walk took for it.
#[derive(Clone, Copy)]
pub(super) struct MusicRefresh<'r> {
    /// The item.
    pub(super) id: Uuid,
    /// What it is.
    pub(super) kind: MusicKind,
    /// Its refresh decision, from its stored row as it stood before this
    /// scan touched it.
    pub(super) plan: ItemRefreshPlan,
    /// The options it refreshes with.
    pub(super) request: RefreshRequest<'r>,
    /// The chosen Identify result, when this refresh identifies the item.
    pub(super) identified: Option<&'r RemoteSearchResult>,
    /// Its library's fetcher checkboxes.
    pub(super) policy: FetcherPolicy<'r>,
    /// Its children-derived metadata is recomputed: upstream's
    /// `UpdateMetadataFromChildren` past its `isFullRefresh || updateType >
    /// None` gate (`MetadataService.cs:404-483`, `AlbumMetadataService.cs:
    /// 77-95`) — a first, required, full or replacing refresh, or one whose
    /// walk saved the item. A child that changed does not make it due. The
    /// cumulative runtime is the folder aggregate pass's, over every folder
    /// the scan planned: upstream derives it only past this gate too
    /// (`MetadataService.cs:451-454`), and deriving it on every validation
    /// is a deliberate, harmless divergence
    /// ([`update_folder_aggregates`](LibraryScanner::update_folder_aggregates)).
    pub(super) aggregate: bool,
    /// This pass stamps the item's `DateLastRefreshed` and writes its new
    /// `DateModified`: the walk left both to it (owner decisions D1 and
    /// D3: a refresh that fails or never runs leaves the item due).
    pub(super) owns_stamp: bool,
    /// This pass decides whether the item is saved at all (`SaveInternal`):
    /// no walk refreshed it (a by-name artist).
    pub(super) owns_save: bool,
    /// The item's folder mtime as the walk found it: the `DateModified`
    /// `SaveInternal` stamps (`MetadataService.cs:244-255`). `None` for an
    /// item with no folder, which keeps its stored value.
    pub(super) date_modified: Option<chrono::DateTime<Utc>>,
}

/// Upstream's `isFullRefresh` (`MetadataService.cs:150`): `isFirstRefresh ||
/// ReplaceAllMetadata || FullRefresh || requiresRefresh || ForceSave`.
pub(super) fn full_refresh(plan: &ItemRefreshPlan, request: &RefreshRequest<'_>) -> bool {
    plan.is_first_refresh
        || plan.requires_refresh
        || request.options.replace_all_metadata
        || request.options.metadata_refresh_mode == MetadataRefreshMode::FullRefresh
        || request.force_save
}

/// Which of the music providers one refresh runs.
// One flag per provider the library's checkboxes gate separately.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy)]
pub(super) struct MusicFetch {
    /// `MusicBrainzAlbumProvider` / `MusicBrainzArtistProvider`.
    musicbrainz: bool,
    /// `AudioDbAlbumProvider` / `AudioDbArtistProvider`.
    audiodb: bool,
    /// A plugin's (Tier-1b WASM) metadata source enabled for the kind.
    dynamic: bool,
    /// `AudioDbAlbumImageProvider` / `AudioDbArtistImageProvider`.
    audiodb_images: bool,
    /// fanart.tv's album/artist images.
    fanart_images: bool,
    /// A named plugin image provider enabled for this item.
    dynamic_images: bool,
    /// How the remote image providers fill the item's images.
    images: ImageFetch,
}

impl MusicFetch {
    /// A remote metadata provider runs.
    fn metadata(self) -> bool {
        self.musicbrainz || self.audiodb || self.dynamic
    }

    /// A remote image provider runs.
    fn any_images(self) -> bool {
        self.audiodb_images || self.fanart_images || self.dynamic_images
    }

    /// A remote provider of either kind runs.
    pub(super) fn any(self) -> bool {
        self.metadata() || self.any_images()
    }
}

/// What the remote metadata providers of one item answered.
#[derive(Default)]
struct MusicAnswer {
    /// The providers' result, when one of them answered
    /// (`RefreshResult.UpdateType` with `MetadataDownload`).
    temp: Option<MetadataResult>,
    /// TheAudioDB's artwork from the same response, when it was asked.
    audiodb_images: Option<Vec<ferrofin_providers::TmdbImage>>,
    /// The MusicBrainz artist id the lookup resolved (an artist's, for its
    /// image providers).
    artist_id: Option<String>,
}

/// A provider id of `name` in `ids`, when valid for its provider
/// (`MusicBrainzId` in `AlbumInfoExtensions.cs:87-88`).
fn valid_id(ids: &[(String, String)], name: &str) -> Option<String> {
    ids.iter()
        .find(|(key, value)| key.eq_ignore_ascii_case(name) && is_valid_provider_id(key, value))
        .map(|(_, value)| value.trim().to_owned())
}

/// The first valid `name` id among `songs`
/// (`info.SongInfos.Select(...).FirstOrDefault(i => !IsNullOrEmpty(i))`,
/// `AlbumInfoExtensions.cs:11-83`).
fn first_song_id(
    songs: &[BaseItemEntity],
    song_ids: &HashMap<Uuid, Vec<(String, String)>>,
    name: &str,
) -> Option<String> {
    songs
        .iter()
        .filter_map(|song| Uuid::parse_str(&song.id).ok())
        .find_map(|id| valid_id(song_ids.get(&id)?, name))
}

/// Sets each valid id of `ids` on `target` that holds no valid id of its
/// name yet: `MergeData`'s provider-id rule with `replaceData = false`
/// (`MetadataService.cs:1307-1328`) — how `ExecuteRemoteProviders` gathers
/// its providers' answers in `temp` — and `MergeNewData`'s (`:1079-1100`),
/// how it hands them to the next provider's lookup info.
pub(super) fn fill_ids(target: &mut Vec<(String, String)>, ids: &[(String, String)]) {
    for (key, value) in ids {
        if is_valid_provider_id(key, value) && valid_id(target, key).is_none() {
            target.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
            target.push((key.clone(), value.clone()));
        }
    }
}

/// The lookup info a plugin's (Tier-1b WASM) metadata source is asked by
/// for `work`'s item, as the built-in providers of its kind are: the name
/// they look up (`name`), the item's year and path, and the ids the lookup
/// holds by its turn (`ids`: the item's own, then each earlier answer's).
fn dynamic_lookup(
    work: &MusicRefresh<'_>,
    item: &BaseItemEntity,
    name: &str,
    ids: &[(String, String)],
) -> ferrofin_traits::providers::DynamicMetadataLookup {
    ferrofin_traits::providers::DynamicMetadataLookup {
        item_id: work.id,
        kind: work.kind.type_name().to_owned(),
        name: name.to_owned(),
        production_year: item.production_year.and_then(|y| i32::try_from(y).ok()),
        path: item.path.clone(),
        provider_ids: ids.to_vec(),
    }
}

/// An empty `temp` row for `item`'s kind: the type is what the kind's merge
/// rules key on (`MergeAlbumArtist` and the album's `Artists`).
fn provider_row(item: &BaseItemEntity) -> BaseItemEntity {
    BaseItemEntity {
        type_: item.type_.clone(),
        ..BaseItemEntity::default()
    }
}

/// The `ResultLanguage` of a TheAudioDB answer: Ferrofin maps its English
/// description (`strDescriptionEN`), which `AudioDbAlbumProvider`/
/// `AudioDbArtistProvider` mark `"en"` so it does not block a provider that
/// can serve the requested language (`AudioDbAlbumProvider.cs:122-131`).
const AUDIODB_LANGUAGE: &str = "en";

/// `values` as a `|`-joined list column; `None` when empty.
fn joined(values: &[String]) -> Option<String> {
    (!values.is_empty()).then(|| values.join("|"))
}

/// A remote metadata provider of music.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MusicSource {
    /// `MusicBrainzAlbumProvider` / `MusicBrainzArtistProvider` (`Order`
    /// 0).
    MusicBrainz,
    /// `AudioDbAlbumProvider` / `AudioDbArtistProvider` (`Order` 1).
    AudioDb,
    /// A plugin's (Tier-1b WASM) metadata source: its index in the scanner's
    /// registration order.
    Dynamic(usize),
}

/// The order the remote providers of a `kind` item run in
/// (`GetMetadataProvidersInternal`, `ProviderManager.cs:523-540`): the
/// admin's `MetadataFetcherOrder` for the kind — the library's, else the
/// server-wide one — then each provider's own `Order` (MusicBrainz 0,
/// TheAudioDB 1, a WASM plugin 50), then registration: the plugins'
/// `dynamic` sources first, as [`LibraryScanner::dynamic_sources`] lists
/// them, then the built-in providers ([`BUILT_IN_METADATA_FETCHERS`], where
/// the three-category tie rule is spelled out). Identifying moves the
/// provider the chosen result came from to the front ("When identifying, run
/// the provider the user picked first so the correct IDs are used",
/// `MetadataService.cs:876-882`, a stable `OrderBy`).
///
/// [`BUILT_IN_METADATA_FETCHERS`]: ferrofin_providers::library_options::BUILT_IN_METADATA_FETCHERS
fn music_sources(
    policy: FetcherPolicy<'_>,
    kind: MusicKind,
    identified: Option<&RemoteSearchResult>,
    dynamic: &[(usize, Option<&str>, i32)],
) -> Vec<MusicSource> {
    use ferrofin_providers::library_options::{BUILT_IN_METADATA_FETCHERS, default_metadata_order};
    let mut candidates: Vec<(MusicSource, Option<&str>, i32)> = dynamic
        .iter()
        .map(|&(index, name, order)| (MusicSource::Dynamic(index), name, order))
        .collect();
    candidates.extend(BUILT_IN_METADATA_FETCHERS.iter().filter_map(|&name| {
        let source = match name {
            fetcher_names::MUSICBRAINZ => MusicSource::MusicBrainz,
            fetcher_names::AUDIODB => MusicSource::AudioDb,
            _ => return None,
        };
        Some((
            source,
            Some(name),
            default_metadata_order(name, kind.type_name()),
        ))
    }));
    provider_order(
        policy,
        kind.type_name(),
        &candidates,
        identified.and_then(|r| r.search_provider_name.as_deref()),
    )
}

/// `AlbumMetadataService.SetProviderIdFromSongs` (`:158-178`) for the three
/// ids an album takes from its tracks (`SetAlbumArtistFromSongs`,
/// `SetAlbumFromSongs`): the tracks' most common value — a track with none
/// counts as a value of its own, so mostly-untagged tracks change nothing —
/// replaces the album's when it is set and differs ignoring case.
fn ids_from_songs(
    ids: &[(String, String)],
    songs: &[BaseItemEntity],
    song_ids: &HashMap<Uuid, Vec<(String, String)>>,
) -> Vec<(String, String)> {
    let mut out = ids.to_vec();
    for name in [
        "MusicBrainzAlbumArtist",
        "MusicBrainzAlbum",
        "MusicBrainzReleaseGroup",
    ] {
        // `GroupBy(i => i).OrderByDescending(g => g.Count())`: stable, so
        // ties keep first-appearance order.
        let mut groups: Vec<(Option<String>, usize)> = Vec::new();
        for song in songs {
            let value = Uuid::parse_str(&song.id).ok().and_then(|id| {
                song_ids.get(&id).and_then(|pairs| {
                    pairs
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                })
            });
            match groups.iter_mut().find(|(key, _)| *key == value) {
                Some((_, count)) => *count += 1,
                None => groups.push((value, 1)),
            }
        }
        groups.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        let Some((Some(first), _)) = groups.into_iter().next() else {
            continue;
        };
        if first.is_empty() {
            continue;
        }
        match out
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            Some((_, current)) if current.eq_ignore_ascii_case(&first) => {}
            Some((_, current)) => *current = first,
            None => out.push((name.to_owned(), first)),
        }
    }
    out
}

/// The provider ids of `a` and `b` are the same set (keys ignoring case).
fn same_ids(a: &[(String, String)], b: &[(String, String)]) -> bool {
    let norm = |ids: &[(String, String)]| {
        let mut v: Vec<(String, String)> = ids
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        v.sort();
        v
    };
    norm(a) == norm(b)
}

/// Whether upstream heals the children-derived columns a "Replace all
/// metadata" merge of this item erased (see [`restore_from_children`]):
/// only when the merge erased its `RunTimeTicks` too — `MergeData` keeps a
/// field in `LockedFields` (`MetadataService.cs:1260-1269`), so a locked
/// `Runtime` survives and the item is never `requiresRefresh` for it — and
/// only for a folder a later scan refreshes and asks `Folder.RequiresRefresh`
/// (`Folder.cs:213-223`): an album or a folder artist. An artist known only
/// by name is refreshed again only while it was never refreshed
/// (`ArtistsValidator.cs:86-91`), so upstream never heals it.
fn heals_from_children(kind: MusicKind, locked_fields: &[MetadataField]) -> bool {
    kind != MusicKind::ByNameArtist && !locked_fields.contains(&MetadataField::Runtime)
}

/// The children-derived columns a "Replace all metadata" merge cleared
/// because the providers returned none of them, put back from the children
/// — when upstream would heal them ([`heals_from_children`]).
///
/// Upstream derives them in `BeforeSave` and then lets `RemoveOldMetadata`
/// erase whatever the providers did not return; the album is healed on the
/// NEXT scan because the erased `RunTimeTicks` makes it `requiresRefresh`
/// (`Folder.cs:213-223`), which derives them again (`UpdateMetadataFromChildren`)
/// and runs every provider a second time. Here the folder aggregate pass
/// rewrites the cumulative runtime in the same scan, so that trigger never
/// comes: the values are put back in this pass instead. This is not a
/// divergence: it is the same end state as upstream's (the providers'
/// values where they answered, the children's elsewhere), reached one scan
/// earlier. With `Runtime` locked upstream never heals, and neither does
/// this.
fn restore_from_children(row: &mut BaseItemEntity, derived: &BaseItemEntity, kind: MusicKind) {
    if row.run_time_ticks.is_none() {
        row.run_time_ticks = derived.run_time_ticks;
    }
    if kind != MusicKind::Album {
        return;
    }
    let empty = |v: &Option<String>| v.as_deref().is_none_or(|v| v.trim().is_empty());
    for (column, source) in [
        (&mut row.genres, &derived.genres),
        (&mut row.studios, &derived.studios),
        (&mut row.artists, &derived.artists),
        (&mut row.album_artists, &derived.album_artists),
    ] {
        if empty(column) && !empty(source) {
            column.clone_from(source);
        }
    }
    if row.premiere_date.is_none() {
        row.premiere_date = derived.premiere_date;
    }
    if row.production_year.is_none() {
        row.production_year = derived.production_year;
    }
}

/// The refresh facts of an item with no file: a music artist, whose
/// `Folder.RequiresRefresh` fires while it has no cumulative runtime.
const ARTIST_FACTS: FileFacts = FileFacts {
    mtime: None,
    probe: ProbeKind::None,
    sidecars_changed: false,
    lyrics_changed: false,
    local_metadata: None,
    supports_cumulative_run_time: true,
    is_shortcut: false,
    is_file_protocol: false,
    is_placeholder: false,
};

/// The decision state of a stored row.
fn stored_state(row: &BaseItemEntity) -> StoredState {
    StoredState {
        date_last_refreshed: row.date_last_refreshed,
        date_last_saved: row.date_last_saved,
        date_modified: row.date_modified,
        run_time_ticks: row.run_time_ticks,
        total_bitrate: row.total_bitrate,
        is_virtual_item: row.is_virtual_item,
        is_locked: row.is_locked,
    }
}

impl LibraryScanner {
    /// The music providers one refresh of a `kind` item runs, as its `plan`
    /// and its library's checkboxes say: a remote metadata provider only
    /// when the plan runs them (never for a locked item), a remote image
    /// provider only when the plan's `GetNonLocalImageProviders` does, and
    /// each only when its client is wired and its library's "Metadata
    /// downloaders" / "Image fetchers" checkbox for the kind is ticked
    /// (`ProviderManager.CanRefreshMetadata`/`CanRefreshImages`) — the
    /// plugins' metadata sources by the same rule
    /// ([`dynamic_sources`](LibraryScanner::dynamic_sources)).
    pub(super) fn music_fetch(
        &self,
        kind: MusicKind,
        plan: &ItemRefreshPlan,
        policy: FetcherPolicy<'_>,
        locked: bool,
    ) -> MusicFetch {
        let name = kind.type_name();
        let metadata = plan.remote_metadata && !locked;
        let images = plan.remote_images != ImageFetch::None
            && self.tmdb.is_some()
            && self.metadata_dir.is_some();
        MusicFetch {
            musicbrainz: metadata
                && self.musicbrainz.is_some()
                && policy.metadata_enabled(name, fetcher_names::MUSICBRAINZ),
            audiodb: metadata
                && self.audiodb.is_some()
                && policy.metadata_enabled(name, fetcher_names::AUDIODB),
            dynamic: metadata && !self.dynamic_sources(policy, name).is_empty(),
            audiodb_images: images
                && self.audiodb.is_some()
                && policy.image_enabled(name, fetcher_names::AUDIODB),
            fanart_images: images
                && self.fanart.is_some()
                && policy.image_enabled(name, fetcher_names::FANART),
            dynamic_images: images
                && self.dynamic_providers.iter().any(|provider| {
                    provider.library_gated() && policy.image_enabled(name, provider.name())
                }),
            images: plan.remote_images,
        }
    }

    /// The music pass over `work`: folder artists first, then albums, then
    /// artists known only by name. Upstream refreshes a folder that is not
    /// an `IMetadataContainer` — a `MusicArtist` (`MusicArtist.cs:27`) —
    /// before it recurses into its children (`Folder.cs:850-876`), so an
    /// album's search already has its artist's MusicBrainz id
    /// (`GetMusicBrainzArtistId`, `AlbumInfoExtensions.cs:50-60`); a by-name
    /// artist is refreshed after the library validation
    /// (`ArtistsValidator`). An item whose decision runs nothing is skipped
    /// without a read. Between two items the lane's refreshes run; an item
    /// one of them refreshed as far as this scan would (`served`,
    /// [`RefreshReach::covers`]) is left alone — it had its own music pass,
    /// and what this scan decided for it is stale. An item that cannot be
    /// read or written is skipped and reported once for the pass; it keeps
    /// what made it due, so the next scan tries it again.
    pub(super) async fn refresh_music(
        &self,
        work: &[MusicRefresh<'_>],
        run: ScanRun<'_>,
        served: &mut Served,
    ) {
        if self.item_repository.is_none() {
            return;
        }
        let mut skipped = super::SkippedItems::default();
        for kind in [MusicKind::Artist, MusicKind::Album, MusicKind::ByNameArtist] {
            for item in work.iter().filter(|w| w.kind == kind) {
                let due = item.aggregate
                    || item.owns_stamp
                    || self
                        .music_fetch(item.kind, &item.plan, item.policy, false)
                        .any();
                if !due {
                    continue;
                }
                if run.cancel.is_cancelled() {
                    break;
                }
                super::note_served(served, &Box::pin(self.serve_lane_reach(run)).await);
                if served
                    .get(&item.id)
                    .is_some_and(|reach| reach.covers(RefreshReach::of(item.request.options)))
                {
                    continue;
                }
                // One item that cannot be read or written is skipped: the
                // items after it still refresh, and it keeps what made it
                // due for the next scan.
                if let Err(err) =
                    Box::pin(self.refresh_music_item(item, run.cancel, &mut skipped)).await
                {
                    skipped.note(item.id, &err);
                }
            }
        }
        skipped.report("music");
    }

    /// Upstream's `ArtistsValidator` (`ArtistsValidator.cs:86-91`), after
    /// every library validation: each artist known only by name that is new
    /// or was never refreshed (`isNew || neverRefreshed`; a new row here is
    /// one never refreshed) refreshes with the default options — its first
    /// refresh, so its providers run once. They are selected by that stamp
    /// in one read, so a refresh that failed (owner decision D1: it leaves
    /// the artist unstamped) is retried by the next validation, and an
    /// unchanged library reads no artist row. The walk refreshed the
    /// folder-backed ones.
    ///
    /// # Errors
    ///
    /// A storage failure listing, reading or writing the artists.
    pub(super) async fn refresh_by_name_artists(
        &self,
        run: ScanRun<'_>,
        outside: FetcherPolicy<'_>,
        served: &mut Served,
    ) -> Result<(), ServiceError> {
        let Some(items) = &self.item_repository else {
            return Ok(());
        };
        let fresh = self
            .persistence
            .never_refreshed_ids(BaseItemKind::MusicArtist, true)
            .await?;
        let defaults = MetadataRefreshOptions::default();
        let request = RefreshRequest {
            options: &defaults,
            force_save: false,
        };
        let now = Utc::now();
        let mut skipped = super::SkippedItems::default();
        for id in fresh {
            if run.cancel.is_cancelled() {
                break;
            }
            super::note_served(served, &Box::pin(self.serve_lane_reach(run)).await);
            if served
                .get(&id)
                .is_some_and(|reach| reach.covers(RefreshReach::of(&defaults)))
            {
                continue;
            }
            let artist = match items.retrieve_item(id).await {
                Ok(Some(artist)) => artist,
                Ok(None) => continue,
                Err(err) => {
                    skipped.note(id, &err);
                    continue;
                }
            };
            let plan = crate::refresh_plan::plan_item_refresh(
                Some(&stored_state(&artist)),
                &ARTIST_FACTS,
                &request,
                None,
                now,
                false,
            );
            let work = MusicRefresh {
                id,
                kind: MusicKind::ByNameArtist,
                plan,
                request,
                identified: None,
                policy: outside,
                aggregate: full_refresh(&plan, &request),
                owns_stamp: true,
                owns_save: true,
                date_modified: None,
            };
            if let Err(err) =
                Box::pin(self.refresh_music_item(&work, run.cancel, &mut skipped)).await
            {
                skipped.note(id, &err);
            }
        }
        skipped.report("by-name artists");
        Ok(())
    }

    /// `POST /Items/{id}/Refresh` or an Identify of an artist known only by
    /// name ([`ScanTarget::Artist`](ferrofin_traits::library::ScanTarget)
    /// with no folder): `item.RefreshMetadata(options)` for that artist —
    /// its providers as the request's options decide, the chosen result's
    /// ids pinned — through the scanner, so it runs inside a running scan
    /// between two items and never beside one.
    ///
    /// # Errors
    ///
    /// A storage failure reading or writing the artist.
    pub(super) async fn refresh_by_name_artist(
        &self,
        id: Uuid,
        run: ScanRun<'_>,
    ) -> Result<ScanOutcome, ServiceError> {
        if let Some(touched) = run.touched {
            touched
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((id, RefreshReach::of(run.options)));
        }
        let Some(items) = &self.item_repository else {
            return Ok(ScanOutcome::default());
        };
        let Some(stored) = items.retrieve_item(id).await? else {
            tracing::warn!(item_id = %id, "artist refresh: the artist is gone; nothing refreshed");
            return Ok(ScanOutcome::default());
        };
        if item_type_lookup::kind_from_type_name(&stored.type_) != Some(BaseItemKind::MusicArtist) {
            return Ok(ScanOutcome::default());
        }
        let request = RefreshRequest {
            options: run.options,
            force_save: run.options.force_save,
        };
        let server = self.server_metadata_options();
        let plan = crate::refresh_plan::plan_item_refresh(
            Some(&stored_state(&stored)),
            &ARTIST_FACTS,
            &request,
            None,
            Utc::now(),
            false,
        );
        let work = MusicRefresh {
            id,
            kind: MusicKind::ByNameArtist,
            plan,
            request,
            identified: run.options.search_result.as_ref(),
            policy: FetcherPolicy {
                options: None,
                global: Some(&server),

                ..Default::default()
            },
            aggregate: full_refresh(&plan, &request),
            owns_stamp: true,
            owns_save: true,
            date_modified: None,
        };
        let mut skipped = super::SkippedItems::default();
        let saved = Box::pin(self.refresh_music_item(&work, run.cancel, &mut skipped)).await;
        skipped.report("artist refresh");
        let saved = saved?;
        Ok(match saved {
            Some(true) => ScanOutcome {
                updated: 1,
                ..ScanOutcome::default()
            },
            Some(false) => ScanOutcome {
                unchanged: 1,
                ..ScanOutcome::default()
            },
            None => ScanOutcome {
                stopped: true,
                ..ScanOutcome::default()
            },
        })
    }

    /// One album's or artist's music refresh: the children-derived metadata
    /// (`BeforeSave` → `UpdateMetadataFromChildren`), the remote metadata
    /// providers merged by the request's mode (`RefreshWithProviders`), the
    /// remote image providers, then the `DateLastRefreshed` stamp and the
    /// save — only of what changed. Returns whether the item was written;
    /// `None` when `cancel` stopped it before it was (nothing of it is then
    /// written).
    ///
    /// # Errors
    ///
    /// A storage failure reading or writing the item.
    // One read-derive-fetch-merge-save sequence, read top to bottom.
    #[allow(clippy::too_many_lines)]
    async fn refresh_music_item(
        &self,
        work: &MusicRefresh<'_>,
        cancel: &ScanCancel,
        skipped: &mut super::SkippedItems,
    ) -> Result<Option<bool>, ServiceError> {
        use ferrofin_providers::rate_limit::count_request_failures;
        let Some(items) = &self.item_repository else {
            return Ok(Some(false));
        };
        // Read now, not at the walk: the walk saved it, and a lane refresh
        // may have since.
        let Some(stored) = items.retrieve_item(work.id).await? else {
            return Ok(Some(false));
        };
        let lookup = super::ResolverGuesses {
            own_language: super::own_language(stored.preferred_metadata_language.as_deref()),
            own_country: super::own_language(stored.preferred_metadata_country_code.as_deref()),
            ..Default::default()
        };
        let locale = self
            .resolve_metadata_locale(
                &stored,
                &lookup,
                work.policy,
                &mut super::ArtworkCache::default(),
            )
            .await?;
        let resolved_work = MusicRefresh {
            policy: FetcherPolicy {
                effective_locale: Some(&locale),
                ..work.policy
            },
            ..*work
        };
        let work = &resolved_work;
        let options = work.request.options;
        let fetch = self.music_fetch(work.kind, &work.plan, work.policy, stored.is_locked);
        // The fields the user locked keep their stored values through the
        // merge; a failed read locks every field rather than risk one.
        let locked_fields = match self.persistence.locked_fields_for_items(&[work.id]).await {
            Ok(mut map) => map.remove(&work.id).unwrap_or_default(),
            Err(err) => {
                skipped.note_locks_unread(work.id, &err);
                ALL_LOCKABLE_FIELDS.to_vec()
            }
        };
        let stored_ids = self
            .persistence
            .provider_ids_for_items(&[work.id])
            .await?
            .remove(&work.id)
            .unwrap_or_default();
        let has_artist_id = valid_id(&stored_ids, "MusicBrainzArtist").is_some();
        let children_needed = match work.kind {
            MusicKind::Album => work.aggregate || fetch.any(),
            MusicKind::Artist => fetch.any() && !has_artist_id,
            // Its tagged items give it its runtime, never an id.
            MusicKind::ByNameArtist => work.aggregate,
        };
        let children = if children_needed {
            items.get_item_list(&Self::music_children(work)).await?
        } else {
            Vec::new()
        };
        let child_ids = if children.is_empty() {
            HashMap::new()
        } else {
            let ids: Vec<Uuid> = children
                .iter()
                .filter_map(|c| Uuid::parse_str(&c.id).ok())
                .collect();
            self.persistence.provider_ids_for_items(&ids).await?
        };

        // `BeforeSave` → `UpdateMetadataFromChildren` (before the providers).
        let mut current = MetadataResult {
            provider_ids: stored_ids.clone(),
            locked_fields: locked_fields.clone(),
            ..MetadataResult::of(stored.clone())
        };
        if work.aggregate {
            match work.kind {
                MusicKind::Album => {
                    let (row, _) = apply_album_child_metadata(&stored, &children, &locked_fields);
                    current.item = row;
                    // Below `AlbumMetadataService`'s `IsLocked` return
                    // (`:77-80`), as the rest of its tag aggregation.
                    if !stored.is_locked {
                        current.provider_ids =
                            ids_from_songs(&current.provider_ids, &children, &child_ids);
                    }
                }
                // `GetTaggedItems(IsFolder = false)` (`ArtistMetadataService.cs:
                // 45-53`); a folder-backed artist's runtime is the folder
                // aggregate pass's.
                MusicKind::ByNameArtist => {
                    current.item.run_time_ticks = Some(
                        children
                            .iter()
                            .filter(|c| !c.is_folder)
                            .map(|c| c.run_time_ticks.unwrap_or(0))
                            .sum(),
                    );
                }
                MusicKind::Artist => {}
            }
        }
        // The chosen Identify result's ids are the item's (the controller's
        // `item.SetProviderIds`); the tracks' tags never override them.
        if let Some(result) = work.identified {
            current.provider_ids =
                merge_provider_ids(&search_result_ids(result), &current.provider_ids, true);
        }
        let derived = current.item.clone();

        // `RefreshWithProviders`: the remote providers, their failures
        // counted (a provider that fails keeps the item unstamped, D1).
        let (answer, failures) = if fetch.metadata() {
            let parent_ids = if work.kind == MusicKind::Album
                && valid_id(&current.provider_ids, "MusicBrainzAlbumArtist").is_none()
            {
                self.music_artist_ids(&stored, current.item.album_artists.as_deref())
                    .await?
            } else {
                Vec::new()
            };
            let fetched = cancel
                .unless_cancelled(count_request_failures(Box::pin(self.fetch_music_answer(
                    work,
                    fetch,
                    &current,
                    &children,
                    &child_ids,
                    &parent_ids,
                ))))
                .await;
            let Some(fetched) = fetched else {
                return Ok(None);
            };
            fetched
        } else {
            (MusicAnswer::default(), 0)
        };
        let answered = answer.temp.is_some();
        let merged = match answer.temp {
            Some(mut temp) if !stored.is_locked => {
                // What upstream's `temp` starts with (`MetadataService.cs:
                // 794-798`).
                temp.item.id.clone_from(&stored.id);
                temp.item.type_.clone_from(&stored.type_);
                temp.item.path.clone_from(&stored.path);
                temp.item
                    .preferred_metadata_language
                    .clone_from(&stored.preferred_metadata_language);
                temp.item
                    .preferred_metadata_country_code
                    .clone_from(&stored.preferred_metadata_country_code);
                let merge = RefreshMerge::of(
                    options,
                    RefreshAnswers {
                        any: true,
                        failed: failures > 0,
                        remote: true,
                        local_locked: false,
                    },
                );
                merge_refresh(&current, temp, &locked_fields, merge)
            }
            _ => current,
        };
        let mut row = merged.item;
        let ids = merged.provider_ids;
        // The merge ran (a provider answered for an unlocked item).
        if answered && !stored.is_locked && heals_from_children(work.kind, &locked_fields) {
            restore_from_children(&mut row, &derived, work.kind);
        }
        // A file fact, not a provider's: `SaveInternal` stamps the folder's
        // mtime (`MetadataService.cs:244-255`) — the walk's, which it left
        // to this pass when the refresh was this pass's to complete.
        row.date_modified = work.date_modified.or(stored.date_modified);
        settle_sort_name(&mut row);

        // The remote image providers, keyed by the ids just settled.
        let (images, image_failures) = if fetch.any_images() {
            let artist_key = answer
                .artist_id
                .clone()
                .or_else(|| valid_id(&ids, "MusicBrainzArtist"));
            let fetched = cancel
                .unless_cancelled(count_request_failures(Box::pin(self.music_images(
                    work,
                    &row,
                    fetch,
                    &ids,
                    artist_key,
                    answer.audiodb_images,
                ))))
                .await;
            let Some(fetched) = fetched else {
                return Ok(None);
            };
            fetched
        } else {
            (None, 0)
        };

        // `RefreshMetadata`'s tail: the stamp and `SaveInternal`.
        let row_changed = row != stored;
        // The item-value index mirrors five of the row's columns only.
        let values = item_values_of(&row);
        let values_changed = values != item_values_of(&stored);
        let ids_changed = !same_ids(&ids, &stored_ids);
        let changed = row_changed || ids_changed || images.is_some();
        let decision = crate::refresh_plan::decide_save(
            work.plan,
            &work.request,
            PassOutcome {
                changed,
                failed: failures + image_failures > 0,
            },
        );
        let stamp = work.owns_stamp && decision.stamp_refreshed;
        let save = if work.owns_save {
            decision.save
        } else {
            changed || stamp
        };
        tracing::debug!(
            item_id = %work.id,
            kind = ?work.kind,
            first = work.plan.is_first_refresh,
            requires = work.plan.requires_refresh,
            aggregate = work.aggregate,
            musicbrainz = fetch.musicbrainz,
            audiodb = fetch.audiodb,
            dynamic = fetch.dynamic,
            images = fetch.any_images(),
            answered,
            failures = failures + image_failures,
            save,
            stamp,
            "music refresh decided"
        );
        if !save {
            return Ok(Some(false));
        }
        // The row — its `DateLastRefreshed` stamp and new `DateModified`
        // with it — is written last: a write that fails before it leaves the
        // item as due as it was, so the next scan refreshes it again.
        if values_changed {
            self.persistence.save_item_values(work.id, &values).await?;
        }
        if ids_changed {
            self.persistence.replace_provider_ids(work.id, &ids).await?;
        }
        let saved_images = images.is_some();
        if let Some(images) = images {
            self.persistence.save_item_images(work.id, &images).await?;
        }
        if stamp {
            row.date_last_refreshed = Some(Utc::now());
        }
        self.persistence
            .save_items(std::slice::from_ref(&row))
            .await?;
        self.save_metadata_sidecars(
            work.id,
            if answered || work.request.force_save || work.request.options.replace_all_metadata {
                ItemUpdateType::MetadataDownload
            } else if saved_images {
                ItemUpdateType::ImageUpdate
            } else {
                ItemUpdateType::MetadataImport
            },
        )
        .await;
        Ok(Some(true))
    }

    /// `MusicAlbum.GetMusicArtist` (`MusicAlbum.cs:69-87`), whose provider
    /// ids are the album lookup's `ArtistProviderIds` (`GetLookupInfo`,
    /// `:142-147`): the first `MusicArtist` among the album's parents,
    /// walked up from its own; else the artist named by the album's first
    /// album artist — `LibraryManager.GetArtist(name)` → `CreateItemByName`
    /// (`LibraryManager.cs:1348-1362`): the `MusicArtist` rows of that raw
    /// name (`UseRawName`), a folder artist before one known only by name
    /// (`OrderBy(i => i.IsAccessedByName ? 1 : 0)`). A parent that cannot be
    /// read ends the walk.
    ///
    /// # Errors
    ///
    /// A storage failure reading the artist's ids.
    async fn music_artist_ids(
        &self,
        album: &BaseItemEntity,
        album_artists: Option<&str>,
    ) -> Result<Vec<(String, String)>, ServiceError> {
        let Some(items) = &self.item_repository else {
            return Ok(Vec::new());
        };
        let mut seen = HashSet::new();
        let mut parent = album
            .parent_id
            .as_deref()
            .and_then(|p| Uuid::parse_str(p).ok());
        let mut artist = None;
        while let Some(id) = parent.filter(|id| seen.insert(*id)) {
            let Ok(Some(row)) = items.retrieve_item(id).await else {
                break;
            };
            if item_type_lookup::kind_from_type_name(&row.type_) == Some(BaseItemKind::MusicArtist)
            {
                artist = Some(id);
                break;
            }
            parent = row
                .parent_id
                .as_deref()
                .and_then(|p| Uuid::parse_str(p).ok());
        }
        if artist.is_none()
            && let Some(name) = split_pipe(album_artists).into_iter().next()
        {
            let named = items
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::MusicArtist],
                    name: Some(name),
                    use_raw_name: Some(true),
                    ..InternalItemsQuery::default()
                })
                .await?;
            // `IsAccessedByName` is `ParentId.IsEmpty()`.
            let by_name = |a: &&BaseItemEntity| a.parent_id.as_deref().is_none_or(str::is_empty);
            artist = named
                .iter()
                .find(|a| !by_name(a))
                .or_else(|| named.iter().find(by_name))
                .and_then(|a| Uuid::parse_str(&a.id).ok());
        }
        let Some(artist) = artist else {
            return Ok(Vec::new());
        };
        Ok(self
            .persistence
            .provider_ids_for_items(&[artist])
            .await?
            .remove(&artist)
            .unwrap_or_default())
    }

    /// The query for `work`'s children: an album's tracks (recursive, so a
    /// multi-disc album aggregates), a folder artist's tracks, a by-name
    /// artist's tagged items.
    fn music_children(work: &MusicRefresh<'_>) -> InternalItemsQuery {
        match work.kind {
            MusicKind::Album => InternalItemsQuery {
                parent_id: work.id,
                include_item_types: vec![BaseItemKind::Audio],
                recursive: true,
                ..InternalItemsQuery::default()
            },
            MusicKind::Artist => InternalItemsQuery {
                ancestor_ids: vec![work.id],
                include_item_types: vec![BaseItemKind::Audio],
                recursive: true,
                ..InternalItemsQuery::default()
            },
            MusicKind::ByNameArtist => InternalItemsQuery {
                artist_ids: vec![work.id],
                is_folder: Some(false),
                recursive: true,
                ..InternalItemsQuery::default()
            },
        }
    }

    /// The plugins' (Tier-1b WASM) metadata sources `work`'s refresh runs
    /// ([`LibraryScanner::dynamic_sources`]): none unless `fetch` runs them.
    fn music_dynamic_sources(
        &self,
        work: &MusicRefresh<'_>,
        fetch: MusicFetch,
    ) -> Vec<(usize, Option<&str>, i32)> {
        if fetch.dynamic {
            self.dynamic_sources(work.policy, work.kind.type_name())
        } else {
            Vec::new()
        }
    }

    /// `RefreshWithProviders`' state when its remote providers start, for
    /// `work`'s item: the lookup info holding the item's ids (`id`), and
    /// `temp` holding what the item's local reader found — its `album.nfo`
    /// or `artist.nfo` (`AlbumNfoProvider`/`ArtistNfoProvider`), merged in
    /// first (`MergeData(localItem, temp, [], false, true)`,
    /// `MetadataService.cs:803-850`), its ids with it — so every remote
    /// provider, built-in or plugin, only fills what the NFO left
    /// (`MergeData(result, temp, [], false, false)`, `:1003`) and the NFO's
    /// values replace the stored ones as the pass merges `temp` back.
    ///
    /// The local reader runs where the item's decision runs the local
    /// readers, and never under an Identify: upstream tests the refresh's
    /// own options (`options.SearchResult is null`, `:803`), and those carry
    /// the chosen result to every child of the identified folder as well
    /// (`ProviderManager.RefreshItem` → `ValidateChildren(options)`, the
    /// options copy keeping `SearchResult`) — an album under an identified
    /// artist reads no `album.nfo` either, as the walk's `run_local` already
    /// has it (`search.is_none()`). `work.identified` is set for the
    /// identified item alone, so the scan's options decide here. Upstream
    /// also skips it under "Replace all metadata" when the
    /// item has a metadata saver (`isSavingMetadata`, the
    /// `ProviderManager.GetMetadataSavers(...).Any()` of `:184`): Ferrofin
    /// runs no saver yet (the open "NFO metadata saver is never run" item),
    /// so under "Replace all metadata" the NFO seeds `temp` as it does in the
    /// walk, which is upstream's behaviour for a library without a saver.
    async fn music_fold_start<'p>(
        &self,
        work: &MusicRefresh<'_>,
        current: &MetadataResult,
        preferred: &'p str,
    ) -> (RemoteFold<'p>, BaseItemEntity) {
        let mut fold = RemoteFold::new(&current.provider_ids, preferred);
        let mut temp = provider_row(&current.item);
        if work.request.options.search_result.is_some() || !work.plan.local_metadata {
            return (fold, temp);
        }
        let mut local = BaseItemEntity {
            id: current.item.id.clone(),
            type_: current.item.type_.clone(),
            path: current.item.path.clone(),
            is_folder: current.item.is_folder,
            ..BaseItemEntity::default()
        };
        let nfo = self.fetch_local_nfo(&mut local, work.policy).await;
        if nfo.found {
            temp = local;
            fill_ids(&mut fold.lookup, &nfo.ids);
            fold.result.provider_ids = nfo.ids;
        }
        (fold, temp)
    }

    /// The remote metadata providers of one item, in the admin's order
    /// ([`music_sources`]), each filling only what an earlier one left
    /// empty (`ExecuteRemoteProviders` merges with `replaceData = false`) and
    /// handing the next one the ids it found (`MergeNewData`).
    async fn fetch_music_answer(
        &self,
        work: &MusicRefresh<'_>,
        fetch: MusicFetch,
        current: &MetadataResult,
        children: &[BaseItemEntity],
        child_ids: &HashMap<Uuid, Vec<(String, String)>>,
        parent_ids: &[(String, String)],
    ) -> MusicAnswer {
        match work.kind {
            MusicKind::Album => {
                self.fetch_album_answer(work, fetch, current, children, child_ids, parent_ids)
                    .await
            }
            MusicKind::Artist | MusicKind::ByNameArtist => {
                self.fetch_artist_answer(work, fetch, current, children, child_ids)
                    .await
            }
        }
    }

    /// `MusicBrainzAlbumProvider` + `AudioDbAlbumProvider`, and the plugins'
    /// metadata sources at their rank among them.
    ///
    /// The lookup name is the Identify's choice, else the first non-empty
    /// `Album` tag of the tracks, else the album's name (`MusicAlbum.
    /// GetLookupInfo`, `MusicAlbum.cs:154-161`). The release is looked up by
    /// the album's own ids, else its tracks' (`GetReleaseId`/
    /// `GetReleaseGroupId`), and searched by name and artist only when there
    /// is no release — a stored match is never searched again. The match is
    /// then looked up, as upstream always does, release and release group
    /// both, and `Populate` reads them: the group's first release date else
    /// the release's, the credits as album artists, the genres and tags by
    /// votes, the labels as studios. TheAudioDB is asked by the release
    /// group the lookup info holds when its turn comes.
    // One provider after the other, in the admin's order, read top to
    // bottom.
    #[allow(clippy::too_many_lines)]
    async fn fetch_album_answer(
        &self,
        work: &MusicRefresh<'_>,
        fetch: MusicFetch,
        current: &MetadataResult,
        children: &[BaseItemEntity],
        child_ids: &HashMap<Uuid, Vec<(String, String)>>,
        parent_ids: &[(String, String)],
    ) -> MusicAnswer {
        // `AlbumInfo`: the album's ids, extended by each provider's answer
        // (`RemoteFold::lookup`); `temp` starts from its `album.nfo`.
        let preferred = preferred_language(&current.item, work.policy);
        let (mut fold, mut temp) = self.music_fold_start(work, current, &preferred).await;
        let mut answered = false;
        let name = work
            .identified
            .and_then(|r| r.name.clone())
            .or_else(|| {
                children
                    .iter()
                    .filter_map(|c| c.album.as_deref())
                    .find(|a| !a.is_empty())
                    .map(ToOwned::to_owned)
            })
            .or_else(|| current.item.name.clone())
            .unwrap_or_default();
        // `GetAlbumArtist`: the tracks' first album artist, else the album's.
        let album_artist = children
            .iter()
            .flat_map(|c| split_pipe(c.album_artists.as_deref()))
            .find(|a| !a.is_empty())
            .or_else(|| {
                split_pipe(current.item.album_artists.as_deref())
                    .into_iter()
                    .next()
            });
        let mut audiodb_images = None;
        let dynamic = self.music_dynamic_sources(work, fetch);
        for source in music_sources(work.policy, work.kind, work.identified, &dynamic) {
            match source {
                MusicSource::MusicBrainz => {
                    let (true, Some(mb)) = (fetch.musicbrainz, &self.musicbrainz) else {
                        continue;
                    };
                    let release = valid_id(&fold.lookup, "MusicBrainzAlbum")
                        .or_else(|| first_song_id(children, child_ids, "MusicBrainzAlbum"));
                    let group = valid_id(&fold.lookup, "MusicBrainzReleaseGroup")
                        .or_else(|| first_song_id(children, child_ids, "MusicBrainzReleaseGroup"));
                    // `GetMusicBrainzArtistId`: the album's album-artist id,
                    // its artist's id, its tracks' album-artist id.
                    let artist_id = valid_id(&fold.lookup, "MusicBrainzAlbumArtist")
                        .or_else(|| valid_id(parent_ids, "MusicBrainzArtist"))
                        .or_else(|| first_song_id(children, child_ids, "MusicBrainzAlbumArtist"));
                    let resolved = mb
                        .resolve_album(
                            &name,
                            ferrofin_providers::AlbumIds {
                                release_id: release,
                                release_group_id: group,
                            },
                            artist_id.as_deref(),
                            album_artist.as_deref(),
                        )
                        .await;
                    // The release and its group looked up, and `Populate`
                    // over them. A match neither lookup finds is no answer
                    // (`if (release is null && releaseGroup is null) return
                    // result;`).
                    let Some(details) = mb.album_details(resolved).await else {
                        continue;
                    };
                    answered = true;
                    // MusicBrainz names no `ResultLanguage`.
                    let mut result = RemoteAnswer::for_row(&current.item, None);
                    let item = &mut result.item;
                    item.premiere_date = details
                        .premiere_date
                        .and_then(ferrofin_providers::PartialDate::to_utc);
                    item.production_year = details.production_year.map(i64::from);
                    item.album_artists = joined(&details.album_artists);
                    item.genres = joined(&details.genres);
                    item.tags = joined(&details.tags);
                    item.studios = joined(&details.studios);
                    for (key, id) in [
                        ("MusicBrainzAlbum", details.release_id),
                        ("MusicBrainzReleaseGroup", details.release_group_id),
                    ] {
                        if let Some(id) = id {
                            result.provider_ids.push((key.to_owned(), id));
                        }
                    }
                    fold.take(&mut temp, result);
                }
                MusicSource::AudioDb => {
                    // `AudioDbAlbumProvider.GetMetadata`: `info.GetReleaseGroupId()`.
                    let group = valid_id(&fold.lookup, "MusicBrainzReleaseGroup")
                        .or_else(|| first_song_id(children, child_ids, "MusicBrainzReleaseGroup"));
                    let (true, Some(adb), Some(group)) = (
                        fetch.audiodb || fetch.audiodb_images,
                        &self.audiodb,
                        group.as_deref(),
                    ) else {
                        continue;
                    };
                    let album = adb.album(group).await;
                    if fetch.audiodb
                        && let Some(album) = &album
                    {
                        answered = true;
                        let mut result =
                            RemoteAnswer::for_row(&current.item, Some(AUDIODB_LANGUAGE));
                        let item = &mut result.item;
                        // `ProcessResult`: with `ReplaceAlbumName` (off by
                        // default) the album's name. ACCEPTED DIVERGENCE
                        // (Ferrofin makes the documented option work;
                        // upstream's sets `item.Album`, which no merge
                        // copies — a no-op): see `AudioDbAlbum::name`.
                        item.name.clone_from(&album.name);
                        item.album_artists.clone_from(&album.artist);
                        item.overview.clone_from(&album.description);
                        item.production_year = album.year.map(i64::from);
                        item.genres.clone_from(&album.genre);
                        result.provider_ids = [
                            ("AudioDbArtist", &album.audiodb_artist_id),
                            ("AudioDbAlbum", &album.audiodb_album_id),
                            ("MusicBrainzAlbumArtist", &album.musicbrainz_artist_id),
                            (
                                "MusicBrainzReleaseGroup",
                                &album.musicbrainz_release_group_id,
                            ),
                        ]
                        .into_iter()
                        .filter_map(|(key, value)| Some((key.to_owned(), value.clone()?)))
                        .collect();
                        fold.take(&mut temp, result);
                    }
                    audiodb_images = Some(album.map(|a| a.images).unwrap_or_default());
                }
                MusicSource::Dynamic(index) => {
                    let Some(result) = self
                        .ask_dynamic(
                            index,
                            &dynamic_lookup(work, &current.item, &name, &fold.lookup),
                            &current.item,
                        )
                        .await
                    else {
                        continue;
                    };
                    answered = true;
                    fold.take(&mut temp, result);
                }
            }
        }
        MusicAnswer {
            temp: answered.then(|| MetadataResult {
                provider_ids: fold.result.provider_ids,
                ..MetadataResult::of(temp)
            }),
            audiodb_images,
            artist_id: None,
        }
    }

    /// `MusicBrainzArtistProvider` + `AudioDbArtistProvider`, and the
    /// plugins' metadata sources at their rank among them.
    ///
    /// The artist is looked up by its own `MusicBrainzArtist` id — a folder
    /// artist's else by the first valid `MusicBrainzAlbumArtist` id among
    /// all the tracks below it (`SongInfos` is every recursive `Audio` child,
    /// `MusicArtist.cs:162-172`; `GetMusicBrainzArtistId`,
    /// `AlbumInfoExtensions.cs:70-82`), whatever name they credit — and
    /// searched by name only when there is neither: a stored match is never
    /// searched again. An artist known only by name has no children to read
    /// ids from (`MusicArtist.Children` is empty for it, `MusicArtist.cs:
    /// 60-69`, so its `SongInfos` are), so only its own id spares the
    /// search. The match is then looked up, as upstream always does, for its
    /// life span, area, genres and tags and, with `ReplaceArtistName`,
    /// MusicBrainz's spelling of its name.
    // One provider after the other, in the admin's order, read top to
    // bottom.
    #[allow(clippy::too_many_lines)]
    async fn fetch_artist_answer(
        &self,
        work: &MusicRefresh<'_>,
        fetch: MusicFetch,
        current: &MetadataResult,
        children: &[BaseItemEntity],
        child_ids: &HashMap<Uuid, Vec<(String, String)>>,
    ) -> MusicAnswer {
        // `ArtistInfo`: the artist's ids, extended by each provider's answer
        // (`RemoteFold::lookup`); `temp` starts from its `artist.nfo`.
        let preferred = preferred_language(&current.item, work.policy);
        let (mut fold, mut temp) = self.music_fold_start(work, current, &preferred).await;
        let mut answered = false;
        let name = work
            .identified
            .and_then(|r| r.name.clone())
            .or_else(|| current.item.name.clone())
            .unwrap_or_default();
        let songs: &[BaseItemEntity] = match work.kind {
            MusicKind::Artist => children,
            MusicKind::Album | MusicKind::ByNameArtist => &[],
        };
        // `ArtistInfo.GetMusicBrainzArtistId`.
        let artist_id = |lookup: &[(String, String)]| {
            valid_id(lookup, "MusicBrainzArtist")
                .or_else(|| first_song_id(songs, child_ids, "MusicBrainzAlbumArtist"))
        };
        let mut audiodb_images = None;
        let dynamic = self.music_dynamic_sources(work, fetch);
        for source in music_sources(work.policy, work.kind, work.identified, &dynamic) {
            match source {
                MusicSource::MusicBrainz => {
                    let (true, Some(mb)) = (fetch.musicbrainz, &self.musicbrainz) else {
                        continue;
                    };
                    let mut id = artist_id(&fold.lookup);
                    let mut searched_name = None;
                    if id.is_none()
                        && !name.is_empty()
                        && let Some(hit) = mb.search_artist_match(&name).await
                    {
                        id = Some(hit.id);
                        searched_name = hit.name;
                    }
                    // `LookupArtistOrNullAsync`: an artist the lookup does not
                    // find is no answer.
                    let Some(id) = id else {
                        continue;
                    };
                    let Some(details) = mb.artist_details(&id).await else {
                        continue;
                    };
                    answered = true;
                    // MusicBrainz names no `ResultLanguage`.
                    let mut result = RemoteAnswer::for_row(&current.item, None);
                    let item = &mut result.item;
                    if mb.replace_artist_name().await {
                        item.name = details.name.or(searched_name);
                    }
                    if let Some(begin) = details.premiere_date {
                        item.premiere_date = begin.to_utc();
                        item.production_year = Some(i64::from(begin.year));
                    }
                    item.end_date = details
                        .end_date
                        .and_then(ferrofin_providers::PartialDate::to_utc);
                    item.production_locations.clone_from(&details.location);
                    item.genres = joined(&details.genres);
                    item.tags = joined(&details.tags);
                    result.provider_ids = vec![("MusicBrainzArtist".to_owned(), id)];
                    fold.take(&mut temp, result);
                }
                MusicSource::AudioDb => {
                    let id = artist_id(&fold.lookup);
                    let (true, Some(adb), Some(id)) = (
                        fetch.audiodb || fetch.audiodb_images,
                        &self.audiodb,
                        id.as_deref(),
                    ) else {
                        continue;
                    };
                    let artist = adb.artist(id).await;
                    if fetch.audiodb
                        && let Some(artist) = &artist
                    {
                        answered = true;
                        let mut result =
                            RemoteAnswer::for_row(&current.item, Some(AUDIODB_LANGUAGE));
                        let item = &mut result.item;
                        item.overview.clone_from(&artist.biography);
                        // `ProcessResult`: the genre and the sub-genre.
                        let genres: Vec<String> = [&artist.genre, &artist.sub_genre]
                            .into_iter()
                            .filter_map(Clone::clone)
                            .collect();
                        item.genres = joined(&genres);
                        item.production_year = artist.formed_year.map(i64::from);
                        item.production_locations.clone_from(&artist.country);
                        result.provider_ids = [
                            ("AudioDbArtist", &artist.audiodb_id),
                            ("MusicBrainzArtist", &artist.musicbrainz_id),
                        ]
                        .into_iter()
                        .filter_map(|(key, value)| Some((key.to_owned(), value.clone()?)))
                        .collect();
                        fold.take(&mut temp, result);
                    }
                    audiodb_images = Some(artist.map(|a| a.images).unwrap_or_default());
                }
                MusicSource::Dynamic(index) => {
                    let Some(result) = self
                        .ask_dynamic(
                            index,
                            &dynamic_lookup(work, &current.item, &name, &fold.lookup),
                            &current.item,
                        )
                        .await
                    else {
                        continue;
                    };
                    answered = true;
                    fold.take(&mut temp, result);
                }
            }
        }
        let artist_id = artist_id(&fold.lookup);
        MusicAnswer {
            temp: answered.then(|| MetadataResult {
                provider_ids: fold.result.provider_ids,
                ..MetadataResult::of(temp)
            }),
            audiodb_images,
            artist_id,
        }
    }

    /// The remote image providers of one item (TheAudioDB's and fanart's
    /// artwork), as `ItemImageProvider.RefreshImages` applies them: short of
    /// replacing, only the image types the item lacks are downloaded; with
    /// `ReplaceAllImages` every type but the ones stored with the media is
    /// replaced. Returns the item's new image set when it changed.
    // Fetch and rank the two providers before applying the image refresh mode.
    #[allow(clippy::too_many_lines)]
    async fn music_images(
        &self,
        work: &MusicRefresh<'_>,
        row: &BaseItemEntity,
        fetch: MusicFetch,
        ids: &[(String, String)],
        artist_key: Option<String>,
        audiodb: Option<Vec<ferrofin_providers::TmdbImage>>,
    ) -> Option<Vec<ItemImageInfo>> {
        let (Some(tmdb), Some(meta_root), Some(items)) =
            (&self.tmdb, &self.metadata_dir, &self.item_repository)
        else {
            return None;
        };
        let stored = match items.get_image_infos(work.id).await {
            Ok(stored) => stored,
            Err(err) => {
                tracing::warn!(%err, item_id = %work.id, "failed to read the item's images");
                return None;
            }
        };
        let preferences = ferrofin_providers::image_policy::ImageAcquisitionPolicy::new(
            work.policy.options,
            work.kind.type_name(),
        );
        let needs = |name| {
            fetch.images == ImageFetch::All
                || ferrofin_providers::library_options::image_types_for_fetcher(
                    work.kind.type_name(),
                    name,
                )
                .iter()
                .any(|kind| {
                    stored
                        .iter()
                        .filter(|image| image.image_type == *kind)
                        .count()
                        < preferences.limit(*kind)
                })
        };
        let mut found: Vec<ferrofin_providers::TmdbImage> = Vec::new();
        let mut fanart_found = Vec::new();
        match work.kind {
            MusicKind::Album => {
                let group = valid_id(ids, "MusicBrainzReleaseGroup");
                if fetch.audiodb_images && needs(fetcher_names::AUDIODB) {
                    match (audiodb, &self.audiodb, group.as_deref()) {
                        (Some(images), ..) => found.extend(images),
                        (None, Some(adb), Some(group)) => {
                            found.extend(
                                adb.album(group).await.map(|a| a.images).unwrap_or_default(),
                            );
                        }
                        _ => {}
                    }
                }
                if fetch.fanart_images
                    && needs(fetcher_names::FANART)
                    && let (Some(fanart), Some(group), Some(artist)) = (
                        &self.fanart,
                        group.as_deref(),
                        valid_id(ids, "MusicBrainzAlbumArtist"),
                    )
                {
                    let fanart = fanart
                        .as_ref()
                        .clone()
                        .with_language(&work.policy.metadata_language());
                    fanart_found.extend(fanart.album_images(&artist, group).await);
                }
            }
            MusicKind::Artist | MusicKind::ByNameArtist => {
                if fetch.audiodb_images && needs(fetcher_names::AUDIODB) {
                    match (audiodb, &self.audiodb, artist_key.as_deref()) {
                        (Some(images), ..) => found.extend(images),
                        (None, Some(adb), Some(id)) => {
                            found
                                .extend(adb.artist(id).await.map(|a| a.images).unwrap_or_default());
                        }
                        _ => {}
                    }
                }
                if fetch.fanart_images
                    && needs(fetcher_names::FANART)
                    && let (Some(fanart), Some(id)) = (&self.fanart, artist_key.as_deref())
                {
                    let fanart = fanart
                        .as_ref()
                        .clone()
                        .with_language(&work.policy.metadata_language());
                    fanart_found.extend(fanart.artist_images(id).await);
                }
            }
        }
        if found.is_empty() && fanart_found.is_empty() && !fetch.dynamic_images {
            return None;
        }
        let mut remote: Vec<RemoteImage> = Vec::new();
        append_fanart(&mut remote, found);
        let mut fanart_remote = Vec::new();
        append_fanart(&mut fanart_remote, fanart_found);
        let kind = work.kind.type_name();
        let mut sources = vec![
            (fetcher_names::AUDIODB, Some(remote)),
            (fetcher_names::FANART, Some(fanart_remote)),
        ];
        if fetch.dynamic_images {
            sources.extend(
                self.dynamic_providers
                    .iter()
                    .filter(|p| p.library_gated() && work.policy.image_enabled(kind, p.name()))
                    .map(|p| (p.name(), None)),
            );
        }
        sources.sort_by_key(|(name, _)| work.policy.image_order(kind, name));
        let key = work.id.to_string();
        let meta_root = meta_root.resolve();
        let dir = meta_root.join(&key);
        let mut images: Vec<_> = stored
            .iter()
            .filter(|image| {
                fetch.images != ImageFetch::All
                    || !std::path::Path::new(&image.path).starts_with(&meta_root)
            })
            .cloned()
            .collect();
        if fetch.images == ImageFetch::None {
            return None;
        }
        let preferences = ferrofin_providers::image_policy::ImageAcquisitionPolicy::new(
            work.policy.options,
            kind,
        );
        for (name, candidates) in sources {
            if let Some(candidates) = candidates {
                let mut downloaded = super::artwork::acquire_images(
                    tmdb,
                    &dir,
                    candidates,
                    preferences,
                    &images,
                    fetch.images == ImageFetch::All,
                )
                .await;
                self.fill_image_metadata(&mut downloaded).await;
                images.extend(downloaded);
            } else {
                self.apply_dynamic_images(
                    row,
                    &mut images,
                    FetcherPolicy {
                        only_image_provider: Some(name),
                        replace_images: fetch.images == ImageFetch::All,
                        ..work.policy
                    },
                )
                .await;
            }
        }
        if fetch.images == ImageFetch::All {
            super::artwork::prune_old_backdrops(&dir, &images);
            let acquired: HashSet<_> = images.iter().map(|image| image.image_type).collect();
            images.extend(
                stored
                    .iter()
                    .filter(|image| !acquired.contains(&image.image_type))
                    .cloned(),
            );
        }
        self.fill_image_metadata(&mut images).await;
        images_changed(&images, Some(&stored)).then_some(images)
    }
}

#[cfg(test)]
impl LibraryScanner {
    /// The music pass over `items` as a first scan's walk hands them to it:
    /// each never refreshed, refreshing with the default options under
    /// `policy`, its remote providers running when `remote` says so.
    pub(super) async fn music_pass_for_test(
        &self,
        items: &[(Uuid, MusicKind)],
        policy: FetcherPolicy<'_>,
        remote: bool,
    ) {
        let defaults = MetadataRefreshOptions::default();
        let request = RefreshRequest {
            options: &defaults,
            force_save: false,
        };
        let plan = ItemRefreshPlan {
            is_first_refresh: true,
            remote_metadata: remote,
            run_all_providers: remote,
            remote_images: if remote {
                ImageFetch::MissingOnly
            } else {
                ImageFetch::None
            },
            ..ItemRefreshPlan::IDLE
        };
        let work: Vec<MusicRefresh<'_>> = items
            .iter()
            .map(|&(id, kind)| MusicRefresh {
                id,
                kind,
                plan,
                request,
                identified: None,
                policy,
                aggregate: true,
                owns_stamp: remote,
                owns_save: kind == MusicKind::ByNameArtist,
                date_modified: None,
            })
            .collect();
        self.refresh_music(&work, ScanRun::defaults(), &mut Served::new())
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::{MusicKind, heals_from_children, ids_from_songs, restore_from_children, same_ids};
    use ferrofin_db::entities::base_items::BaseItemEntity;
    use ferrofin_model::entities::MetadataField;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn pairs(values: &[(&str, &str)]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// `AlbumMetadataService.SetProviderIdFromSongs` (`:158-178`): the most
    /// common track id wins, a track without one counts as a value of its
    /// own (so mostly-untagged tracks change nothing), and an album id that
    /// differs only in case is kept.
    #[test]
    fn an_album_takes_its_tracks_most_common_ids() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let song = |id: Uuid| BaseItemEntity {
            id: ferrofin_db::store::guid_to_db(id),
            ..BaseItemEntity::default()
        };
        let songs = [song(a), song(b), song(c)];
        let rel = "11111111-1111-1111-1111-111111111111";
        let other = "22222222-2222-2222-2222-222222222222";
        let mut ids = HashMap::new();
        ids.insert(a, pairs(&[("MusicBrainzAlbum", rel)]));
        ids.insert(
            b,
            pairs(&[
                ("MusicBrainzAlbum", rel),
                ("MusicBrainzReleaseGroup", other),
            ]),
        );
        ids.insert(c, pairs(&[("MusicBrainzAlbum", other)]));
        let out = ids_from_songs(&pairs(&[("MusicBrainzAlbum", other)]), &songs, &ids);
        assert!(
            same_ids(&out, &pairs(&[("MusicBrainzAlbum", rel)])),
            "{out:?}"
        );
        // Two of three tracks carry no release group: the untagged group wins.
        assert!(!out.iter().any(|(k, _)| k == "MusicBrainzReleaseGroup"));
        // An album id differing only in case is kept as it is.
        let upper = rel.to_uppercase();
        let out = ids_from_songs(&pairs(&[("MusicBrainzAlbum", &upper)]), &songs, &ids);
        assert!(same_ids(&out, &pairs(&[("MusicBrainzAlbum", &upper)])));
    }

    /// A "Replace all metadata" merge that erased the children-derived
    /// columns gets them back from the children; what a provider returned
    /// stays.
    #[test]
    fn a_replace_all_merge_keeps_the_childrens_values() {
        let derived = BaseItemEntity {
            genres: Some("Jazz".into()),
            album_artists: Some("Miles Davis".into()),
            run_time_ticks: Some(5),
            production_year: Some(1959),
            ..BaseItemEntity::default()
        };
        let mut row = BaseItemEntity {
            production_year: Some(1960),
            ..BaseItemEntity::default()
        };
        restore_from_children(&mut row, &derived, MusicKind::Album);
        assert_eq!(row.genres.as_deref(), Some("Jazz"));
        assert_eq!(row.album_artists.as_deref(), Some("Miles Davis"));
        assert_eq!(row.run_time_ticks, Some(5));
        assert_eq!(row.production_year, Some(1960), "the provider's year stays");
        let mut artist = BaseItemEntity::default();
        restore_from_children(&mut artist, &derived, MusicKind::Artist);
        assert_eq!(artist.run_time_ticks, Some(5));
        assert_eq!(artist.genres, None, "an artist derives no genres here");
    }

    /// Upstream heals a "Replace all metadata" wipe only through the next
    /// scan's `requiresRefresh` on the wiped runtime: never with `Runtime`
    /// locked (the merge keeps it), never for an artist known only by name
    /// (no scan refreshes it again). Only then are the children's values
    /// put back.
    #[test]
    fn the_childrens_values_come_back_only_where_upstream_heals_them() {
        assert!(heals_from_children(MusicKind::Album, &[]));
        assert!(heals_from_children(
            MusicKind::Artist,
            &[MetadataField::Genres]
        ));
        assert!(
            !heals_from_children(MusicKind::Album, &[MetadataField::Runtime]),
            "a locked runtime is never wiped, so never heals"
        );
        assert!(!heals_from_children(
            MusicKind::Artist,
            &[MetadataField::Runtime]
        ));
        assert!(
            !heals_from_children(MusicKind::ByNameArtist, &[]),
            "a by-name artist is not refreshed again by a scan"
        );
    }

    /// `MetadataServiceRefreshTests.RefreshWithProviders_ForeignProviderId_
    /// ReplacedInLookupInfo`, over the scan's port of
    /// `ExecuteRemoteProviders`' per-answer step ([`RemoteFold::take`], whose
    /// lookup half is `MergeNewData`), which music shares: the stored id
    /// cannot be a TMDB one, so the provider that runs next is handed the id
    /// the one before it just found instead of failing on the same bad one.
    /// A usable id already in the lookup stays ("Don't replace existing
    /// Id's").
    ///
    /// [`RemoteFold::take`]: super::RemoteFold::take
    #[test]
    fn refresh_with_providers_foreign_provider_id_replaced_in_lookup_info() {
        let answering = || super::RemoteAnswer {
            item: BaseItemEntity {
                name: Some("Test Movie".into()),
                ..BaseItemEntity::default()
            },
            people: None,
            provider_ids: pairs(&[("Tmdb", "12345")]),
            language: None,
        };
        let mut temp = BaseItemEntity::default();

        let mut fold = super::RemoteFold::new(&pairs(&[("Tmdb", "nm0000123")]), "en");
        fold.take(&mut temp, answering());
        // What the following provider reads: `info.GetProviderId(Tmdb)`.
        assert_eq!(fold.lookup, pairs(&[("Tmdb", "12345")]));

        let mut fold = super::RemoteFold::new(&pairs(&[("Tmdb", "11")]), "en");
        fold.take(&mut temp, answering());
        assert_eq!(fold.lookup, pairs(&[("Tmdb", "11")]));
    }

    /// The music providers run by the saved `MetadataFetcherOrder`, then
    /// `IHasOrder` (MusicBrainz 0, TheAudioDB 1); identifying moves the
    /// chosen result's provider to the front whatever the saved order
    /// ("When identifying, run the provider the user picked first",
    /// `MetadataService.cs:876-882`).
    #[test]
    fn identify_runs_the_chosen_music_provider_first() {
        use super::{FetcherPolicy, MusicSource, music_sources};
        use ferrofin_model::configuration::{LibraryOptions, TypeOptions};
        use ferrofin_model::providers::RemoteSearchResult;
        use ferrofin_providers::library_options::fetcher_names::{AUDIODB, MUSICBRAINZ};
        let saved = |order: [&str; 2]| LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("MusicAlbum".to_owned()),
                metadata_fetchers: order.iter().map(|n| (*n).to_owned()).collect(),
                metadata_fetcher_order: order.iter().map(|n| (*n).to_owned()).collect(),
                ..TypeOptions::default()
            }],
            ..LibraryOptions::default()
        };
        let chosen = |provider: &str| RemoteSearchResult {
            search_provider_name: Some(provider.to_owned()),
            ..RemoteSearchResult::default()
        };
        let audiodb_first = saved([AUDIODB, MUSICBRAINZ]);
        let policy = FetcherPolicy {
            options: Some(&audiodb_first),
            global: None,

            ..Default::default()
        };
        assert_eq!(
            music_sources(policy, MusicKind::Album, None, &[]),
            [MusicSource::AudioDb, MusicSource::MusicBrainz]
        );
        assert_eq!(
            music_sources(policy, MusicKind::Album, Some(&chosen("musicbrainz")), &[]),
            [MusicSource::MusicBrainz, MusicSource::AudioDb]
        );
        let musicbrainz_first = saved([MUSICBRAINZ, AUDIODB]);
        let policy = FetcherPolicy {
            options: Some(&musicbrainz_first),
            global: None,

            ..Default::default()
        };
        assert_eq!(
            music_sources(policy, MusicKind::Album, Some(&chosen(AUDIODB)), &[]),
            [MusicSource::AudioDb, MusicSource::MusicBrainz]
        );
        assert_eq!(
            music_sources(FetcherPolicy::default(), MusicKind::Artist, None, &[]),
            [MusicSource::MusicBrainz, MusicSource::AudioDb],
            "no saved order: MusicBrainz's IHasOrder 0 leads"
        );
    }

    /// A plugin's (Tier-1b WASM) metadata source takes its place among the
    /// music providers like any provider: its rank in the saved order, then
    /// its `IHasOrder` (a WASM plugin's is upstream's 50, after MusicBrainz's
    /// 0 and TheAudioDB's 1), then registration (plugins first, which only a
    /// full tie reaches); one the fetcher lists do not name is never ranked.
    #[test]
    fn a_plugin_source_runs_at_its_rank_among_the_music_providers() {
        use super::{FetcherPolicy, MusicSource, music_sources};
        use ferrofin_model::configuration::{LibraryOptions, TypeOptions};
        use ferrofin_providers::library_options::fetcher_names::{AUDIODB, MUSICBRAINZ};
        let saved = |order: &[&str]| LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("MusicAlbum".to_owned()),
                metadata_fetchers: order.iter().map(|n| (*n).to_owned()).collect(),
                metadata_fetcher_order: order.iter().map(|n| (*n).to_owned()).collect(),
                ..TypeOptions::default()
            }],
            ..LibraryOptions::default()
        };
        let named = [(0, Some("AlbumDb"), 50)];
        let plugin_first = saved(&["AlbumDb", MUSICBRAINZ, AUDIODB]);
        let policy = FetcherPolicy {
            options: Some(&plugin_first),
            global: None,

            ..Default::default()
        };
        assert_eq!(
            music_sources(policy, MusicKind::Album, None, &named),
            [
                MusicSource::Dynamic(0),
                MusicSource::MusicBrainz,
                MusicSource::AudioDb
            ]
        );
        let between = saved(&[MUSICBRAINZ, "albumdb", AUDIODB]);
        let policy = FetcherPolicy {
            options: Some(&between),
            global: None,

            ..Default::default()
        };
        assert_eq!(
            music_sources(policy, MusicKind::Album, None, &named),
            [
                MusicSource::MusicBrainz,
                MusicSource::AudioDb,
                MusicSource::Dynamic(0)
            ],
            "a differently cased order entry is unranked, but still enabled"
        );
        // No saved order: MusicBrainz (0) and TheAudioDB (1) declare an
        // `IHasOrder` below the plugin's 50.
        assert_eq!(
            music_sources(FetcherPolicy::default(), MusicKind::Album, None, &named),
            [
                MusicSource::MusicBrainz,
                MusicSource::AudioDb,
                MusicSource::Dynamic(0)
            ]
        );
        // A source that declares no `provider-info` has no name to rank.
        let unnamed = [(0, None, 50)];
        let names_it = saved(&["AlbumDb", MUSICBRAINZ, AUDIODB]);
        let policy = FetcherPolicy {
            options: Some(&names_it),
            global: None,

            ..Default::default()
        };
        assert_eq!(
            music_sources(policy, MusicKind::Album, None, &unnamed),
            [
                MusicSource::MusicBrainz,
                MusicSource::AudioDb,
                MusicSource::Dynamic(0)
            ]
        );
    }
}
