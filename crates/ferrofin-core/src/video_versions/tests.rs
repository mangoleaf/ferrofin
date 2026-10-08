//! Transliterated from upstream `tests/Jellyfin.Controller.Tests/Entities/
//! BaseItemTests.cs` (the version, media-source-name and owner-date cases):
//! the C# expected values are the oracle. `SetupVersionGroup`'s mocked
//! `LibraryManager` becomes the rows the scan stores for the same group, read
//! through an in-memory [`VersionRowReader`].

use std::collections::HashMap;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::dto::{MediaSourceInfo, MediaSourceType};
use ferrofin_model::entities::{MediaStreamType, VideoType};
use ferrofin_traits::error::ServiceError;
use rstest::rstest;
use uuid::Uuid;

use super::*;

/// Rows in memory, read as the database would answer.
struct Rows(Vec<BaseItemEntity>);

#[async_trait]
impl VersionRowReader for Rows {
    async fn rows_by_id(&self, ids: &[Uuid]) -> Result<Vec<BaseItemEntity>, ServiceError> {
        Ok(self
            .0
            .iter()
            .filter(|r| ids.contains(&row_id(r)))
            .cloned()
            .collect())
    }

    async fn rows_by_primary(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError> {
        let mut map: HashMap<Uuid, Vec<BaseItemEntity>> = HashMap::new();
        for row in &self.0 {
            if let Some(p) = parse_id(row.primary_version_id.as_deref())
                && ids.contains(&p)
            {
                map.entry(p).or_default().push(row.clone());
            }
        }
        Ok(map)
    }

    async fn rows_by_primary_flagged(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<(BaseItemEntity, bool)>>, ServiceError> {
        let pointed_at: HashSet<Uuid> = self
            .0
            .iter()
            .filter_map(|row| {
                parse_id(row.primary_version_id.as_deref()).filter(|p| *p != row_id(row))
            })
            .collect();
        Ok(self
            .rows_by_primary(ids)
            .await?
            .into_iter()
            .map(|(id, rows)| {
                let flagged = rows
                    .into_iter()
                    .map(|row| {
                        let versioned = pointed_at.contains(&row_id(&row));
                        (row, versioned)
                    })
                    .collect();
                (id, flagged)
            })
            .collect())
    }
}

fn video(path: &str) -> BaseItemEntity {
    BaseItemEntity {
        id: guid_to_db(Uuid::new_v4()),
        type_: "MediaBrowser.Controller.Entities.Video".to_owned(),
        path: Some(path.to_owned()),
        media_type: Some("Video".to_owned()),
        sort_name: std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase()),
        ..BaseItemEntity::default()
    }
}

/// Makes `version` a local alternate of `primary`: owned by it and pointing
/// at it, listed in its `LocalAlternateVersions`.
fn local_version(primary: &mut BaseItemEntity, version: &mut BaseItemEntity) {
    version.owner_id = Some(primary.id.clone());
    version.primary_version_id = Some(primary.id.clone());
    let mut paths = data_paths(primary.data.as_deref(), LOCAL_ALTERNATE_VERSIONS);
    paths.push(version.path.clone().unwrap());
    primary.data = Some(serde_json::json!({ "LocalAlternateVersions": paths }).to_string());
}

/// Upstream `SetupVersionGroup`: `Movie.mkv` with two local alternates.
fn version_group() -> (BaseItemEntity, BaseItemEntity, BaseItemEntity) {
    let mut primary = video("/Movies/Movie/Movie.mkv");
    let mut alt1 = video("/Movies/Movie/Movie - 1080p.mkv");
    let mut alt2 = video("/Movies/Movie/Movie - 4K.mkv");
    local_version(&mut primary, &mut alt1);
    local_version(&mut primary, &mut alt2);
    (primary, alt1, alt2)
}

async fn load(all: &[&BaseItemEntity], queried: &BaseItemEntity) -> VersionRows {
    let reader = Rows(all.iter().map(|r| (*r).clone()).collect());
    VersionRows::load(&reader, &[queried]).await.unwrap()
}

fn ids(rows: &[(&BaseItemEntity, MediaSourceType)]) -> Vec<Uuid> {
    rows.iter().map(|(r, _)| row_id(r)).collect()
}

#[tokio::test]
async fn get_alternate_version_returns_matching_local_version() {
    let (primary, alt1, alt2) = version_group();
    let rows = load(&[&primary, &alt1, &alt2], &primary).await;
    assert_eq!(
        rows.alternate_version(&primary, row_id(&alt1))
            .map(|r| &r.id),
        Some(&alt1.id)
    );
    assert_eq!(
        rows.alternate_version(&primary, row_id(&alt2))
            .map(|r| &r.id),
        Some(&alt2.id)
    );
    assert_eq!(
        rows.alternate_version(&primary, row_id(&primary))
            .map(|r| &r.id),
        Some(&primary.id)
    );
    assert!(rows.alternate_version(&primary, Uuid::new_v4()).is_none());
}

#[tokio::test]
async fn get_all_versions_from_any_version_returns_every_version_once() {
    let (primary, alt1, alt2) = version_group();
    for source in [&primary, &alt1, &alt2] {
        let rows = load(&[&primary, &alt1, &alt2], source).await;
        let versions = rows.all_version_ids(source);
        assert_eq!(versions.len(), 3);
        for v in [&primary, &alt1, &alt2] {
            assert!(versions.contains(&row_id(v)));
        }
    }
}

#[tokio::test]
async fn get_all_items_for_media_sources_from_any_version_has_no_duplicates() {
    let (primary, alt1, alt2) = version_group();
    for source in [&primary, &alt1, &alt2] {
        let rows = load(&[&primary, &alt1, &alt2], source).await;
        let items = ids(&rows.items_for_media_sources(source));
        let mut distinct = items.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(items.len(), 3);
        assert_eq!(distinct.len(), 3);
        for v in [&primary, &alt1, &alt2] {
            assert!(items.contains(&row_id(v)));
        }
    }
}

/// The source of the queried version leads, though the primary is wider
/// (the width only orders sources of one path, so it never outranks it).
#[tokio::test]
async fn get_media_sources_defaults_to_the_queried_versions_own_source() {
    let (primary, alt1, alt2) = version_group();
    let widths: HashMap<String, i32> = [
        (primary.id.clone(), 3840),
        (alt1.id.clone(), 1920),
        (alt2.id.clone(), 1920),
    ]
    .into();
    let build = |row: &BaseItemEntity| MediaSourceInfo {
        id: Some(row_id(row).simple().to_string()),
        video_type: Some(VideoType::VideoFile),
        media_streams: vec![ferrofin_model::entities_media::MediaStream {
            stream_type: MediaStreamType::Video,
            width: widths.get(&row.id).copied(),
            ..Default::default()
        }],
        ..MediaSourceInfo::default()
    };
    let rows = load(&[&primary, &alt1, &alt2], &alt1).await;
    assert_eq!(
        rows.media_sources(&alt1, build)[0].id,
        Some(row_id(&alt1).simple().to_string())
    );
    let rows = load(&[&primary, &alt1, &alt2], &primary).await;
    assert_eq!(
        rows.media_sources(&primary, build)[0].id,
        Some(row_id(&primary).simple().to_string())
    );
}

/// `GetOwnedVersionIds` is `[Id, .. GetLocalAlternateVersionIds(this)]`:
/// the primary, then its local versions in their link order.
#[tokio::test]
async fn get_owned_version_ids_covers_every_local_version() {
    let (primary, alt1, alt2) = version_group();
    let rows = load(&[&primary, &alt1, &alt2], &primary).await;
    let mut owned = vec![row_id(&primary)];
    owned.extend(rows.local_version_ids(&primary));
    assert_eq!(owned, vec![row_id(&primary), row_id(&alt1), row_id(&alt2)]);
}

/// A version merged onto the primary is a `Grouping` source, and so is the
/// primary seen from it; the merged version's own local versions are listed
/// too (`GetAllItemsForMediaSources` takes the local versions of every
/// grouped item), as `Default` sources.
#[tokio::test]
async fn a_merged_versions_group_spans_its_local_versions() {
    let (primary, alt1, alt2) = version_group();
    let mut merged = video("/Movies/Other/Movie (Director's Cut).mkv");
    merged.primary_version_id = Some(primary.id.clone());
    let mut merged_local = video("/Movies/Other/Movie (Director's Cut) - 720p.mkv");
    local_version(&mut merged, &mut merged_local);
    let all = [&primary, &alt1, &alt2, &merged, &merged_local];

    let rows = load(&all, &primary).await;
    let group = rows.items_for_media_sources(&primary);
    assert_eq!(
        ids(&group),
        vec![
            row_id(&primary),
            row_id(&merged),
            row_id(&alt1),
            row_id(&alt2),
            row_id(&merged_local)
        ]
    );
    assert_eq!(group[1].1, MediaSourceType::Grouping);
    assert!(
        group
            .iter()
            .filter(|(r, _)| r.id != merged.id)
            .all(|(_, t)| *t == MediaSourceType::Default)
    );

    let rows = load(&all, &merged).await;
    let group = rows.items_for_media_sources(&merged);
    assert_eq!(group.len(), 5);
    assert_eq!(row_id(group[0].0), row_id(&merged));
    let primary_type = group.iter().find(|(r, _)| r.id == primary.id).unwrap().1;
    assert_eq!(primary_type, MediaSourceType::Grouping);
}

/// A video with no versions lists itself alone, reading only its versions.
#[tokio::test]
async fn a_lone_video_is_its_own_only_source() {
    let solo = video("/Movies/Solo/Solo.mkv");
    let rows = load(&[&solo], &solo).await;
    assert_eq!(
        ids(&rows.items_for_media_sources(&solo)),
        vec![row_id(&solo)]
    );
    assert!(rows.local_version_ids(&solo).is_empty());
}

#[rstest]
#[case(
    "/Movies/Ted/Ted.mp4",
    "/Movies/Ted/Ted - Unrated Edition.mp4",
    "Ted",
    "Unrated Edition"
)]
#[case(
    "/Movies/Deadpool 2 (2018)/Deadpool 2 (2018).mkv",
    "/Movies/Deadpool 2 (2018)/Deadpool 2 (2018) - Super Duper Cut.mkv",
    "Deadpool 2 (2018)",
    "Super Duper Cut"
)]
fn get_media_source_name_valid(
    #[case] primary_path: &str,
    #[case] alt_path: &str,
    #[case] name: &str,
    #[case] alt_name: &str,
) {
    // The mock answers `GetLocalAlternateVersionIds` with one id, so the
    // video has local versions: the folder-name fallback applies.
    let primary = video(primary_path);
    let alt = video(alt_path);
    let folder = containing_folder_name(&primary);
    assert_eq!(media_source_name(Some(&folder), &primary, None), name);
    assert_eq!(media_source_name(Some(&folder), &alt, None), alt_name);
}

#[rstest]
// Episode versions share a season folder; the common prefix (not the folder name) yields the label.
// Both files carry a suffix (no bare base name), so the shared "- " must be stripped too.
#[case(
    "Spider-Noir - S01E02 - Wo ist Flint - Greyscale",
    "Spider-Noir - S01E02 - Wo ist Flint - Colorized",
    "Greyscale",
    "Colorized"
)]
// One version is the bare base name; the other is suffixed.
#[case(
    "Spider-Noir - S01E02 - Wo ist Flint",
    "Spider-Noir - S01E02 - Wo ist Flint - Greyscale",
    "Spider-Noir - S01E02 - Wo ist Flint",
    "Greyscale"
)]
// Suffixes share a leading word ("Grey"); the prefix must retreat to the separator, not split it.
#[case(
    "Demo - S01E01 - Greyscale",
    "Demo - S01E01 - Greyish",
    "Greyscale",
    "Greyish"
)]
// Underscore separator.
#[case("Movie (2020)_4K", "Movie (2020)_1080p", "4K", "1080p")]
// Dot separator.
#[case("Movie (2020).UHD", "Movie (2020).1080p", "UHD", "1080p")]
// Resolution variants that share leading digits must retreat to the separator, not yield "p"/"i".
#[case("Movie - 1080p", "Movie - 1080i", "1080p", "1080i")]
// A token shared by the descriptors but separated only by spaces (the resolution) must stay in the
// label: retreat to the '-' delimiter, not the interior space, so the resolution is kept.
#[case(
    "movie (2020) - 2160p Extended",
    "movie (2020) - 2160p Original",
    "2160p Extended",
    "2160p Original"
)]
// Bracketed version labels: the opening bracket is kept in the label.
#[case(
    "Blade Runner (1982) [Final Cut] [1080p HEVC AAC]",
    "Blade Runner (1982) [EE by ADM] [480p HEVC AAC]",
    "[Final Cut] [1080p HEVC AAC]",
    "[EE by ADM] [480p HEVC AAC]"
)]
// Numeric version labels: the dot between the digits is a decimal point, not a delimiter, so the
// prefix retreats past it to the '-' instead of leaving "0" / "11".
#[case(
    "Evangelion 1.0 You Are (Not) Alone (2007) - 1.0",
    "Evangelion 1.0 You Are (Not) Alone (2007) - 1.11",
    "1.0",
    "1.11"
)]
// Numeric labels with no structural delimiter at all fall back to the space boundary.
#[case("Movie (2007) 1.0", "Movie (2007) 1.11", "1.0", "1.11")]
// A dot followed by a non-digit is still a delimiter, even after a digit.
#[case("Movie - Part 1.HDR", "Movie - Part 1.SDR", "HDR", "SDR")]
fn get_media_source_name_common_prefix_valid(
    #[case] primary_name: &str,
    #[case] alt_name: &str,
    #[case] expected_primary: &str,
    #[case] expected_alt: &str,
) {
    let primary = video(&format!("/Shows/Demo/Season 01/{primary_name}.mkv"));
    let alt = video(&format!("/Shows/Demo/Season 01/{alt_name}.mkv"));
    let prefix = common_version_prefix(&[primary_name, alt_name]);
    // No local alternate versions: these are linked (separate items), so the
    // folder fallback is unavailable.
    assert_eq!(
        media_source_name(None, &primary, Some(&prefix)),
        expected_primary
    );
    assert_eq!(media_source_name(None, &alt, Some(&prefix)), expected_alt);
    // The same names from the group's own prefix (`GetCommonNamePrefix`).
    assert_eq!(
        common_name_prefix(&[&primary, &alt]).unwrap_or_default(),
        prefix
    );
}

#[test]
fn get_common_version_prefix_numeric_labels_keeps_whole_number() {
    // Three versions labelled "1.0", "1.01" and "1.11": the common prefix stops inside the version
    // number, so it must retreat past the decimal point to the '-' delimiter.
    let file_names = [
        "Evangelion 1.0 You Are (Not) Alone (2007) - 1.0",
        "Evangelion 1.0 You Are (Not) Alone (2007) - 1.01",
        "Evangelion 1.0 You Are (Not) Alone (2007) - 1.11",
    ];
    let prefix = common_version_prefix(&file_names);
    assert_eq!(prefix, "Evangelion 1.0 You Are (Not) Alone (2007) -");
    let labels: Vec<&str> = file_names
        .iter()
        .map(|n| n[prefix.len()..].trim_start_matches(' '))
        .collect();
    assert_eq!(labels, ["1.0", "1.01", "1.11"]);
}

/// A local version group's sources are named by what differs: the primary
/// keeps its whole name, each version its label (`GetMediaSources` over
/// `GetCommonNamePrefix`).
#[tokio::test]
async fn a_version_groups_sources_are_named_by_their_labels() {
    let (primary, alt1, alt2) = version_group();
    let rows = load(&[&primary, &alt1, &alt2], &primary).await;
    let mut names: Vec<String> = rows
        .media_sources(&primary, |row| MediaSourceInfo {
            id: Some(row_id(row).simple().to_string()),
            ..MediaSourceInfo::default()
        })
        .into_iter()
        .filter_map(|s| s.name)
        .collect();
    names.sort();
    assert_eq!(names, ["1080p", "4K", "Movie"]);
}

fn year(owner: Option<i64>, date: Option<&str>) -> BaseItemEntity {
    BaseItemEntity {
        production_year: owner,
        premiere_date: date.map(|d| d.parse().unwrap()),
        ..BaseItemEntity::default()
    }
}

#[test]
fn inherit_dates_from_owner_owner_has_dates_overwrites_owned_item_dates() {
    let owner = year(Some(1982), Some("1982-06-25T00:00:00Z"));
    // 2016 is what the container creation date of a re-encoded trailer would have yielded.
    let mut trailer = year(Some(2016), Some("2016-05-04T00:00:00Z"));
    assert!(inherit_dates_from_owner(&owner, &mut trailer));
    assert_eq!(trailer.production_year, owner.production_year);
    assert_eq!(trailer.premiere_date, owner.premiere_date);
}

#[test]
fn inherit_dates_from_owner_owner_has_no_dates_keeps_owned_item_dates() {
    let owner = year(None, None);
    let mut trailer = year(Some(1982), Some("1982-06-25T00:00:00Z"));
    assert!(!inherit_dates_from_owner(&owner, &mut trailer));
    assert_eq!(trailer.production_year, Some(1982));
    assert_eq!(
        trailer.premiere_date,
        Some("1982-06-25T00:00:00Z".parse().unwrap())
    );
}

#[test]
fn inherit_dates_from_owner_dates_already_match_reports_no_change() {
    let owner = year(Some(1982), Some("1982-06-25T00:00:00Z"));
    let mut trailer = year(Some(1982), Some("1982-06-25T00:00:00Z"));
    assert!(!inherit_dates_from_owner(&owner, &mut trailer));
}

#[test]
fn inherit_dates_from_owner_owned_item_has_no_dates_takes_owner_dates() {
    let owner = year(Some(1982), Some("1982-06-25T00:00:00Z"));
    let mut trailer = year(None, None);
    assert!(inherit_dates_from_owner(&owner, &mut trailer));
    assert_eq!(trailer.production_year, Some(1982));
    assert_eq!(
        trailer.premiere_date,
        Some("1982-06-25T00:00:00Z".parse().unwrap())
    );
}

/// `copyTitleMetadata`: the part takes the listed fields, keeps its own
/// name and its date where the owner has none, and a second copy is a no-op.
#[test]
fn copy_title_metadata_takes_the_owners_title_fields() {
    let owner = BaseItemEntity {
        name: Some("Heat".to_owned()),
        genres: Some("Crime|Drama".to_owned()),
        studios: Some("Warner".to_owned()),
        production_locations: Some("USA".to_owned()),
        community_rating: Some(8.3),
        critic_rating: Some(87.0),
        overview: Some("A heist.".to_owned()),
        official_rating: Some("R".to_owned()),
        custom_rating: Some("Adults".to_owned()),
        production_year: Some(1995),
        tagline: Some("Not copied".to_owned()),
        ..BaseItemEntity::default()
    };
    let mut part = BaseItemEntity {
        name: Some("Heat cd2".to_owned()),
        premiere_date: Some("1995-12-15T00:00:00Z".parse().unwrap()),
        ..BaseItemEntity::default()
    };
    assert!(copy_title_metadata(&owner, &mut part));
    assert_eq!(part.name.as_deref(), Some("Heat cd2"));
    assert_eq!(part.genres, owner.genres);
    assert_eq!(part.studios, owner.studios);
    assert_eq!(part.production_locations, owner.production_locations);
    assert_eq!(part.community_rating, owner.community_rating);
    assert_eq!(part.critic_rating, owner.critic_rating);
    assert_eq!(part.overview, owner.overview);
    assert_eq!(part.official_rating, owner.official_rating);
    assert_eq!(part.custom_rating, owner.custom_rating);
    assert_eq!(part.production_year, Some(1995));
    assert_eq!(
        part.premiere_date,
        Some("1995-12-15T00:00:00Z".parse().unwrap()),
        "the owner has no premiere date to give"
    );
    assert_eq!(part.tagline, None);
    assert!(!copy_title_metadata(&owner, &mut part));
}

/// `Video.UpdateToRepositoryAsync`'s columns: taken as they are, an empty
/// value included; critic rating and studios are not among them.
#[test]
fn copy_version_metadata_takes_the_primarys_columns() {
    let primary = BaseItemEntity {
        overview: Some("A heist.".to_owned()),
        production_year: Some(1995),
        premiere_date: None,
        community_rating: Some(8.3),
        official_rating: Some("R".to_owned()),
        genres: Some("Crime".to_owned()),
        critic_rating: Some(87.0),
        ..BaseItemEntity::default()
    };
    let mut version = BaseItemEntity {
        overview: Some("Its own overview".to_owned()),
        premiere_date: Some("1995-12-15T00:00:00Z".parse().unwrap()),
        studios: Some("Kept".to_owned()),
        ..BaseItemEntity::default()
    };
    assert!(copy_version_metadata(&primary, &mut version));
    assert_eq!(version.overview, primary.overview);
    assert_eq!(version.production_year, Some(1995));
    assert_eq!(version.premiere_date, None);
    assert_eq!(version.community_rating, Some(8.3));
    assert_eq!(version.official_rating.as_deref(), Some("R"));
    assert_eq!(version.genres.as_deref(), Some("Crime"));
    assert_eq!(version.critic_rating, None);
    assert_eq!(version.studios.as_deref(), Some("Kept"));
    assert!(!copy_version_metadata(&primary, &mut version));
}

#[rstest]
#[case(None, 0)]
#[case(Some(r#"{"VideoType":"VideoFile"}"#), 0)]
#[case(Some(r#"{"AdditionalParts":[]}"#), 0)]
#[case(Some(r#"{"AdditionalParts":null}"#), 0)]
#[case(Some(r#"{"AdditionalParts":["/m/b.mkv","/m/c.mkv"]}"#), 2)]
#[case(Some(r#"{"AdditionalParts" : [ ] ,"X":1}"#), 0)]
#[case(
    Some(r#"{"VideoType":"VideoFile", "AdditionalParts": ["/m/b.mkv"]}"#),
    1
)]
fn additional_part_count_reads_the_list(#[case] data: Option<&str>, #[case] count: usize) {
    assert_eq!(additional_part_count(data), count);
}

/// `BaseItemTests.SupportsOwnedItems_EpisodeWithResolvedVersionOrPart_IsTrue`
/// (`Episode.SupportsOwnedItems`, `Episode.cs:50`: `IsStacked ||
/// LocalAlternateVersions.Length > 0 || MediaSourceCount > 1`): a version
/// the scan just found beside an episode is not linked yet, so it does not
/// count towards `MediaSourceCount`; the episode still owns — refreshes and
/// copies its metadata to — the videos its resolved lists name.
#[rstest]
#[case(true, false, true)]
#[case(false, true, true)]
#[case(false, false, false)]
fn an_episode_with_a_resolved_version_or_part_owns_videos(
    #[case] has_local_version: bool,
    #[case] is_stacked: bool,
    #[case] expected: bool,
) {
    let versions: &[&str] = if has_local_version {
        &["/TV/Show/Season 1/S01E01 - 720p.mkv"]
    } else {
        &[]
    };
    let parts: &[&str] = if is_stacked {
        &["/TV/Show/Season 1/S01E01 - 1080p-part2.mkv"]
    } else {
        &[]
    };
    let data = serde_json::json!({
        "AdditionalParts": parts,
        "LocalAlternateVersions": versions,
    })
    .to_string();
    assert_eq!(owns_videos(Some(&data)), expected);
}

/// [`Rows`], counting the reads.
struct Counting(Rows, std::sync::atomic::AtomicUsize);

#[async_trait]
impl VersionRowReader for Counting {
    async fn rows_by_id(&self, ids: &[Uuid]) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.0.rows_by_id(ids).await
    }

    async fn rows_by_primary(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<BaseItemEntity>>, ServiceError> {
        self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.0.rows_by_primary(ids).await
    }

    async fn rows_by_primary_flagged(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<(BaseItemEntity, bool)>>, ServiceError> {
        self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.0.rows_by_primary_flagged(ids).await
    }
}

async fn reads(all: &[&BaseItemEntity], queried: &[&BaseItemEntity]) -> usize {
    let reader = Counting(
        Rows(all.iter().map(|r| (*r).clone()).collect()),
        std::sync::atomic::AtomicUsize::new(0),
    );
    VersionRows::load(&reader, queried).await.unwrap();
    reader.1.into_inner()
}

/// The page read is pinned: one query for a page with no versions on it,
/// and for a page whose versions have none of their own (each read flags
/// the rows something points at); one more for a version whose primary is
/// off the page; one more per level of versions that have versions (a
/// merged version's local versions, or a merge left on a version a scan
/// later grouped under another primary).
#[tokio::test]
async fn a_page_reads_its_version_groups_in_as_few_queries_as_it_needs() {
    let solo = video("/Movies/Solo/Solo.mkv");
    let other = video("/Movies/Other/Other.mkv");
    assert_eq!(reads(&[&solo, &other], &[&solo, &other]).await, 1);

    let (primary, alt1, alt2) = version_group();
    let all = [&primary, &alt1, &alt2];
    assert_eq!(reads(&all, &[&primary]).await, 1, "a primary on the page");
    assert_eq!(
        reads(&all, &[&alt1]).await,
        2,
        "a version, its primary off the page"
    );
    assert_eq!(reads(&all, &[&primary, &alt1]).await, 1, "both on the page");

    let mut merged = video("/Movies/Cut/Cut.mkv");
    merged.primary_version_id = Some(primary.id.clone());
    let with_merged = [&primary, &alt1, &alt2, &merged];
    assert_eq!(
        reads(&with_merged, &[&primary]).await,
        1,
        "a merged version without local versions"
    );
    let mut merged_local = video("/Movies/Cut/Cut - 720p.mkv");
    local_version(&mut merged, &mut merged_local);
    let all = [&primary, &alt1, &alt2, &merged, &merged_local];
    assert_eq!(
        reads(&all, &[&primary]).await,
        2,
        "a merged version's own local versions"
    );
}

/// A group three levels deep — a version merged onto a version that a scan
/// later grouped as another primary's local version — lists every row from
/// any of its members, and reaches the chain's root.
#[tokio::test]
async fn a_three_level_chain_lists_the_whole_group_from_any_member() {
    let mut head = video("/TV/Show/Season 1/Show - S01E01 - 1080p.mkv");
    let mut local = video("/TV/Show/Season 1/Show - S01E01 - 720p.mkv");
    local_version(&mut head, &mut local);
    let mut merged = video("/TV2/Show/Season 1/Show - S01E01.mkv");
    merged.primary_version_id = Some(local.id.clone());
    let all = [&head, &local, &merged];
    let ids = |rows: &[&BaseItemEntity]| {
        let mut ids: Vec<Uuid> = rows.iter().map(|r| row_id(r)).collect();
        ids.sort();
        ids
    };
    for queried in all {
        let reader = Rows(all.iter().map(|r| (*r).clone()).collect());
        let versions = VersionRows::load(&reader, &[queried]).await.unwrap();
        let mut found = versions.all_version_ids(queried);
        found.sort();
        assert_eq!(
            found,
            ids(&all),
            "from {}",
            queried.path.as_deref().unwrap()
        );
        assert_eq!(versions.chain_root(queried), row_id(&head));
    }
}

/// `SetAlternateVersionResumeStates` over `SelectMostRecentlyPlayed`: the
/// resumable version played last leads; a completed one (no position) does
/// not move; a queried version keeps its own source first.
#[test]
fn the_version_being_resumed_leads_a_primarys_sources() {
    use ferrofin_model::dto::UserItemDataDto;
    let (primary, alt1, alt2) = version_group();
    let source = |row: &BaseItemEntity| MediaSourceInfo {
        id: Some(row_id(row).simple().to_string()),
        ..MediaSourceInfo::default()
    };
    let at = |position: i64, day: u32| UserItemDataDto {
        rating: None,
        played_percentage: None,
        unplayed_item_count: None,
        playback_position_ticks: position,
        play_count: 1,
        is_favorite: false,
        likes: None,
        last_played_date: Some(
            chrono::NaiveDate::from_ymd_opt(2026, 1, day)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc(),
        ),
        played: false,
        key: String::new(),
        item_id: Uuid::nil(),
    };
    let data: HashMap<Uuid, UserItemDataDto> = [
        (row_id(&alt1), at(10, 1)),
        (row_id(&alt2), at(20, 2)),
        (row_id(&primary), at(0, 3)),
    ]
    .into();
    let ids = |sources: &[MediaSourceInfo]| -> Vec<String> {
        sources.iter().map(|s| s.id.clone().unwrap()).collect()
    };
    let mut sources = vec![source(&primary), source(&alt1), source(&alt2)];
    put_resumed_version_first(&primary, &mut sources, |id| data.get(&id));
    assert_eq!(
        ids(&sources),
        ids(&[source(&alt2), source(&primary), source(&alt1)])
    );

    let mut sources = vec![source(&alt1), source(&primary), source(&alt2)];
    put_resumed_version_first(&alt1, &mut sources, |id| data.get(&id));
    assert_eq!(
        sources[0].id,
        source(&alt1).id,
        "a queried version keeps its own"
    );

    let none: HashMap<Uuid, UserItemDataDto> = [(row_id(&alt1), at(0, 9))].into();
    let mut sources = vec![source(&primary), source(&alt1)];
    put_resumed_version_first(&primary, &mut sources, |id| none.get(&id));
    assert_eq!(
        sources[0].id,
        source(&primary).id,
        "a completed version stays put"
    );
}

/// `GetMediaSourceName`'s `Video` terms (`BaseItem.cs:1340-1375`): `3D` for
/// a `Video3DFormat`, then `Bluray`/`DVD` for a disc — or a disc image of
/// that `IsoType` — and `ISO` for an image of no known kind, each joined to
/// the name with `/`. A `VideoFile` adds nothing.
#[rstest]
#[case::video_file(
    "/m/Heat (1995)/Heat (1995).mkv",
    r#"{"VideoType":"VideoFile"}"#,
    "Heat (1995)"
)]
#[case::no_video_type("/m/Heat (1995)/Heat (1995).mkv", r"{}", "Heat (1995)")]
#[case::dvd("/m/Alien (1979)", r#"{"VideoType":"Dvd"}"#, "Alien (1979)/DVD")]
#[case::bluray(
    "/m/Avatar (2009)",
    r#"{"VideoType":"BluRay"}"#,
    "Avatar (2009)/Bluray"
)]
#[case::iso_dvd("/m/Film.iso", r#"{"VideoType":"Iso","IsoType":"Dvd"}"#, "Film/DVD")]
#[case::iso_bluray(
    "/m/Film.iso",
    r#"{"VideoType":"Iso","IsoType":"BluRay"}"#,
    "Film/Bluray"
)]
#[case::iso_unknown("/m/Film.iso", r#"{"VideoType":"Iso"}"#, "Film/ISO")]
#[case::iso_null_kind("/m/Film.iso", r#"{"VideoType":"Iso","IsoType":null}"#, "Film/ISO")]
#[case::three_d(
    "/m/Film.3D.mkv",
    r#"{"VideoType":"VideoFile","Video3DFormat":"HalfSideBySide"}"#,
    "Film.3D/3D"
)]
#[case::three_d_bluray(
    "/m/Film",
    r#"{"VideoType":"BluRay","Video3DFormat":"MVC"}"#,
    "Film/3D/Bluray"
)]
fn get_media_source_name_names_the_disc(
    #[case] path: &str,
    #[case] data: &str,
    #[case] expected: &str,
) {
    let mut item = video(path);
    item.data = Some(data.to_owned());
    assert_eq!(media_source_name(None, &item, None), expected);
}

/// `Video.ContainingFolderPath` (`Video.cs:201-219`): a stacked video's is its
/// first part's folder — a multi-disc set's folder — a disc rip's its own
/// path, a `.disc` placeholder's and a file's their folder.
#[rstest]
#[case::file("/m/Heat/Heat.mkv", false, r#"{"VideoType":"VideoFile"}"#, "/m/Heat")]
#[case::dvd_rip("/m/Alien", false, r#"{"VideoType":"Dvd"}"#, "/m/Alien")]
#[case::bluray_rip("/m/Avatar", false, r#"{"VideoType":"BluRay"}"#, "/m/Avatar")]
#[case::placeholder(
    "/m/Alien/Alien.disc",
    false,
    r#"{"VideoType":"Dvd","IsPlaceHolder":true}"#,
    "/m/Alien"
)]
#[case::multi_disc(
    "/m/Set/Set - Disc 1",
    false,
    r#"{"VideoType":"Dvd","AdditionalParts":["/m/Set/Set - Disc 2"]}"#,
    "/m/Set"
)]
#[case::folder("/m/Show", true, r"{}", "/m/Show")]
fn containing_folder_path_follows_the_video_type(
    #[case] path: &str,
    #[case] is_folder: bool,
    #[case] data: &str,
    #[case] expected: &str,
) {
    let mut item = video(path);
    item.is_folder = is_folder;
    item.data = Some(data.to_owned());
    assert_eq!(containing_folder_path(&item), expected);
}
