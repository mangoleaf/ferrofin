//! The guide refresh's `BaseItems` mirror works one channel at a time.
//!
//! `GuideManager.RefreshChannelsInternal` (v12.4
//! src/Jellyfin.LiveTv/Guide/GuideManager.cs:232-339) reads a channel's
//! existing programmes (`ChannelIds = [currentChannel.Id]`), merges and stores
//! them, then moves on, so a large guide is never in memory whole. These tests
//! watch the item repository the mirror reads through.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::entities::{ImageType, MediaStreamType};
use ferrofin_model::querying::{QueryFiltersLegacy, QueryResult};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::{InternalItemsQuery, ItemImageInfo};
use ferrofin_traits::persistence::{ItemRepository, ItemWithCounts, PlaylistItemsWithAccess};
use ferrofin_traits::stubs::{LiveTvChannelQuery, LiveTvManager as _};
use uuid::Uuid;

use super::{ArtServices, M3U, guide_day, item_lab_reading_through};
use crate::projection::PROGRAM_TYPE_NAME;

/// One airing: `(guide channel, start hour, end hour, title, icon URL)`.
type Airing = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

/// Two airings on each of the fixture lineup's two channels, tomorrow.
const AIRINGS: [Airing; 4] = [
    (
        "one.tv",
        "06",
        "07",
        "One Early",
        "http://img/one-early.png",
    ),
    ("one.tv", "07", "08", "One Late", "http://img/one-late.png"),
    (
        "two.tv",
        "06",
        "07",
        "Two Early",
        "http://img/two-early.png",
    ),
    ("two.tv", "07", "08", "Two Late", "http://img/two-late.png"),
];

/// A guide listing both of the fixture lineup's channels, carrying `airings`
/// on `day` (a [`guide_day`]), each with a genre and an icon.
fn guide(day: &str, airings: &[Airing]) -> String {
    let mut xml = String::from(
        "<tv>\
         <channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
         <channel id=\"two.tv\"><display-name>Channel Two</display-name></channel>",
    );
    for (channel, from, to, title, icon) in airings {
        let _ = write!(
            xml,
            "<programme start=\"{day}{from}0000 +0000\" stop=\"{day}{to}0000 +0000\" \
             channel=\"{channel}\"><title>{title}</title><category>News</category>\
             <icon src=\"{icon}\"/></programme>"
        );
    }
    xml.push_str("</tv>");
    xml
}

/// An [`ItemRepository`] over the real one that records, for every read
/// returning programme items, the channels those items belong to.
///
/// It forwards every method without a default body. The ones with one
/// (`build_access_filter`, the folder-children trio, the version batches,
/// `get_item_id_clean_names`, …) keep the trait's default, which the guide
/// refresh never reaches; forward one here if that changes.
struct ChannelReads {
    inner: Arc<dyn ItemRepository>,
    reads: Mutex<Vec<BTreeSet<String>>>,
}

impl ChannelReads {
    fn record(&self, rows: &[BaseItemEntity]) {
        let channels: BTreeSet<String> = rows
            .iter()
            .filter(|row| row.type_ == PROGRAM_TYPE_NAME)
            .map(|row| row.channel_id.clone().unwrap_or_default())
            .collect();
        if !channels.is_empty() {
            self.reads.lock().expect("reads").push(channels);
        }
    }

    fn reads(&self) -> Vec<BTreeSet<String>> {
        self.reads.lock().expect("reads").clone()
    }
}

#[async_trait]
impl ItemRepository for ChannelReads {
    async fn retrieve_item(&self, id: Uuid) -> Result<Option<BaseItemEntity>, ServiceError> {
        let row = self.inner.retrieve_item(id).await?;
        self.record(row.as_slice());
        Ok(row)
    }
    async fn retrieve_items(&self, ids: &[Uuid]) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let rows = self.inner.retrieve_items(ids).await?;
        self.record(&rows);
        Ok(rows)
    }
    async fn visible_library_ids(&self, user: &UserEntity) -> Result<Vec<Uuid>, ServiceError> {
        self.inner.visible_library_ids(user).await
    }
    async fn locked_item_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        self.inner.locked_item_ids().await
    }
    async fn get_ancestor_chain(
        &self,
        item_id: Uuid,
    ) -> Result<Option<Vec<BaseItemEntity>>, ServiceError> {
        self.inner.get_ancestor_chain(item_id).await
    }
    async fn get_items(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<BaseItemEntity>, ServiceError> {
        let page = self.inner.get_items(filter).await?;
        self.record(&page.items);
        Ok(page)
    }
    async fn get_item_ids(&self, filter: &InternalItemsQuery) -> Result<Vec<Uuid>, ServiceError> {
        self.inner.get_item_ids(filter).await
    }
    async fn get_item_list(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let rows = self.inner.get_item_list(filter).await?;
        self.record(&rows);
        Ok(rows)
    }
    async fn get_latest_item_list(
        &self,
        filter: &InternalItemsQuery,
        collection_type: ferrofin_model::data::CollectionType,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.inner
            .get_latest_item_list(filter, collection_type)
            .await
    }
    async fn item_exists(&self, id: Uuid) -> Result<bool, ServiceError> {
        self.inner.item_exists(id).await
    }
    async fn get_items_by_primary_version(
        &self,
        primary_id: Uuid,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        self.inner.get_items_by_primary_version(primary_id).await
    }
    async fn get_items_with_provider_id(
        &self,
        provider_key: &str,
    ) -> Result<Vec<(Uuid, String)>, ServiceError> {
        self.inner.get_items_with_provider_id(provider_key).await
    }
    async fn get_image_infos(&self, item_id: Uuid) -> Result<Vec<ItemImageInfo>, ServiceError> {
        self.inner.get_image_infos(item_id).await
    }
    async fn get_image_infos_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<ItemImageInfo>>, ServiceError> {
        self.inner.get_image_infos_batch(item_ids).await
    }
    async fn swap_item_images(
        &self,
        item_id: Uuid,
        image_type: ImageType,
        index1: i32,
        index2: i32,
    ) -> Result<bool, ServiceError> {
        self.inner
            .swap_item_images(item_id, image_type, index1, index2)
            .await
    }
    async fn get_genres(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.inner.get_genres(filter).await
    }
    async fn get_music_genres(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.inner.get_music_genres(filter).await
    }
    async fn get_studios(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.inner.get_studios(filter).await
    }
    async fn get_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.inner.get_artists(filter).await
    }
    async fn get_album_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.inner.get_album_artists(filter).await
    }
    async fn get_all_artists(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryResult<ItemWithCounts>, ServiceError> {
        self.inner.get_all_artists(filter).await
    }
    async fn get_music_genre_names(&self) -> Result<Vec<String>, ServiceError> {
        self.inner.get_music_genre_names().await
    }
    async fn get_studio_names(&self) -> Result<Vec<String>, ServiceError> {
        self.inner.get_studio_names().await
    }
    async fn get_genre_names(&self) -> Result<Vec<String>, ServiceError> {
        self.inner.get_genre_names().await
    }
    async fn get_all_artist_names(&self) -> Result<Vec<String>, ServiceError> {
        self.inner.get_all_artist_names().await
    }
    async fn get_media_stream_languages(
        &self,
        filter: &InternalItemsQuery,
        stream_type: MediaStreamType,
    ) -> Result<Vec<String>, ServiceError> {
        self.inner
            .get_media_stream_languages(filter, stream_type)
            .await
    }
    async fn get_query_filters_legacy(
        &self,
        filter: &InternalItemsQuery,
    ) -> Result<QueryFiltersLegacy, ServiceError> {
        self.inner.get_query_filters_legacy(filter).await
    }
    async fn get_is_played(
        &self,
        user: &UserEntity,
        id: Uuid,
        recursive: bool,
    ) -> Result<bool, ServiceError> {
        self.inner.get_is_played(user, id, recursive).await
    }
    async fn get_playlist_items_with_access(
        &self,
        playlist_id: Uuid,
        user_id: Uuid,
        child_type: i32,
    ) -> Result<PlaylistItemsWithAccess, ServiceError> {
        self.inner
            .get_playlist_items_with_access(playlist_id, user_id, child_type)
            .await
    }
}

/// A refresh that finds the guide already stored reads it back one channel at
/// a time, as upstream loads `existingPrograms` per channel, and still mirrors
/// every airing under its own channel.
///
/// The second pass is the one that matters: on a first import there is
/// nothing stored to read, and before this a refresh read every stored
/// programme item in one `get_item_list`.
#[tokio::test]
async fn a_guide_refresh_reads_the_stored_airings_one_channel_at_a_time() {
    let mut recorder = None;
    let lab = item_lab_reading_through(M3U, Some(guide(&guide_day(0), &AIRINGS)), |inner| {
        let reads = Arc::new(ChannelReads {
            inner,
            reads: Mutex::new(Vec::new()),
        });
        recorder = Some(Arc::clone(&reads));
        reads
    })
    .await;
    let reads = recorder.expect("the lab wraps its item reads");
    lab.mgr.refresh_guide().await.expect("first refresh");
    reads.reads.lock().expect("reads").clear();

    lab.mgr.refresh_guide().await.expect("second refresh");
    let seen = reads.reads();
    assert!(
        seen.iter().all(|channels| channels.len() == 1),
        "one read of the stored airings spanned several channels: {seen:?}"
    );
    let channels: BTreeSet<&String> = seen.iter().flatten().collect();
    assert_eq!(
        channels.len(),
        2,
        "both channels' airings were read back: {seen:?}"
    );

    // The mirror is still the whole guide, each airing under its own channel.
    let stored = crate::guide_repository::test_support::stored_program_items(&lab.mgr.db)
        .await
        .expect("programme items");
    assert_eq!(stored.len(), 4, "{stored:?}");
    let rows = crate::guide_repository::test_support::program_rows(&lab.mgr.db)
        .await
        .expect("guide rows");
    for (id, parent_id, _, _) in &stored {
        let row = rows
            .iter()
            .find(|row| &row.id == id)
            .expect("every item is a guide row");
        assert_eq!(parent_id.as_ref(), Some(&row.channel_id));
    }
    let links = crate::guide_repository::test_support::program_genre_links(&lab.mgr.db)
        .await
        .expect("genre links");
    assert_eq!(links.len(), 4, "every airing keeps its genre: {links:?}");
}

/// One channel's airings are read through the `("ChannelId", "StartDate")`
/// index, so the per-channel mirror never scans the whole guide per channel.
#[tokio::test]
async fn one_channels_airings_are_read_through_the_channel_index() {
    let db = Database::connect_in_memory().await.expect("db");
    db.run_migrations().await.expect("migrate");
    let plan = crate::guide_repository::test_support::channel_program_rows_plan(
        &db,
        "00000000-0000-0000-0000-000000000001",
    )
    .await
    .expect("plan");
    assert!(
        plan.iter().any(|step| step
            == "SEARCH p USING INDEX FerrofinIX_LiveTvPrograms_ChannelId_StartDate (ChannelId=?)"),
        "{plan:?}"
    );
    assert!(
        !plan
            .iter()
            .any(|step| step.starts_with("SCAN p") || step.starts_with("SCAN c")),
        "{plan:?}"
    );
}

/// A logo that is not a URL, from the playlist or the guide, becomes no image,
/// and a refresh takes out the row an earlier version stored from it. A rolling
/// update can store it after a one-shot cleanup would have run, so this happens
/// on every refresh.
#[rstest::rstest]
#[case::playlist(" tvg-logo=\"/var/lib/art/one.png\"", None)]
#[case::guide(
    "",
    Some(
        "<tv><channel id=\"one.tv\"><display-name>One</display-name>\
         <icon src=\"/var/lib/art/one.png\"/></channel></tv>"
    )
)]
#[tokio::test]
async fn a_logo_that_is_no_url_leaves_no_row_and_takes_the_stored_one(
    #[case] playlist_logo: &str,
    #[case] xmltv: Option<&str>,
) {
    let logo = "/var/lib/art/one.png";
    let lab = item_lab_reading_through(
        &format!(
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"one.tv\" tvg-chno=\"1\"{playlist_logo},One\n\
             http://tuner/one\n"
        ),
        xmltv.map(str::to_owned),
        |inner| inner,
    )
    .await;
    lab.mgr.refresh_guide().await.expect("first refresh");
    let channels = crate::guide_repository::channel_rows(
        &lab.mgr.db,
        &LiveTvChannelQuery::default(),
        None,
        false,
    )
    .await
    .expect("channels");
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].image_url.as_deref(), Some(logo));
    let channel = Uuid::parse_str(&channels[0].id).expect("guid");
    assert!(
        lab.items
            .get_image_infos(channel)
            .await
            .expect("images")
            .is_empty(),
        "the logo is no image"
    );

    let stored = ItemImageInfo {
        path: logo.to_owned(),
        image_type: ImageType::Primary,
        ..ItemImageInfo::default()
    };
    lab.persistence
        .save_items_images(&[(channel, vec![stored])])
        .await
        .expect("row of an earlier version");
    lab.mgr.refresh_guide().await.expect("second refresh");
    assert!(
        lab.items
            .get_image_infos(channel)
            .await
            .expect("images")
            .is_empty(),
        "the refresh removed the row"
    );
}

/// A channel whose airings all left the guide has nothing to mirror, so its
/// turn is skipped and its items go in the `CleanDatabase` at the end; the
/// other channel's items, images and genres are untouched.
#[tokio::test]
async fn a_channel_whose_airings_all_left_the_guide_loses_only_its_items() {
    // One day for both guides: an airing's id carries its start.
    let day = guide_day(0);
    let lab = item_lab_reading_through(M3U, Some(guide(&day, &AIRINGS)), |inner| inner).await;
    lab.mgr.refresh_guide().await.expect("first refresh");
    let rows = crate::guide_repository::test_support::program_rows(&lab.mgr.db)
        .await
        .expect("guide rows");
    let mut channel_one: Vec<String> = rows
        .iter()
        .filter(|row| row.title.starts_with("One "))
        .map(|row| row.id.clone())
        .collect();
    channel_one.sort();
    assert_eq!(channel_one.len(), 2);

    // The next guide still lists channel two, but none of its airings. The
    // cached copy goes, as it would once its hour is up.
    lab.sources.0.lock().expect("sources").insert(
        "http://guide/xmltv.xml".to_owned(),
        guide(&day, &AIRINGS[..2]),
    );
    let providers = lab.mgr.get_listing_providers().await.expect("providers");
    let provider = providers[0].id.as_deref().expect("provider id");
    tokio::fs::remove_file(lab.mgr.xmltv.cache_file_for_test(provider))
        .await
        .expect("cached guide");
    lab.mgr.refresh_guide().await.expect("second refresh");

    let stored: Vec<String> =
        crate::guide_repository::test_support::stored_program_items(&lab.mgr.db)
            .await
            .expect("programme items")
            .into_iter()
            .map(|(id, ..)| id)
            .collect();
    assert_eq!(stored, channel_one, "only channel one's airings remain");
    for id in &stored {
        let images = lab
            .items
            .get_image_infos(Uuid::parse_str(id).expect("guid"))
            .await
            .expect("images");
        assert_eq!(images.len(), 1, "{id} keeps its icon: {images:?}");
        assert_eq!(images[0].image_type, ImageType::Primary);
    }
    let linked: Vec<String> =
        crate::guide_repository::test_support::program_genre_links(&lab.mgr.db)
            .await
            .expect("genre links")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
    assert_eq!(linked, channel_one, "channel one keeps its genre links");
}

/// The pre-cache reads each channel's images in its own turn but downloads a
/// URL once for the whole pass: two channels whose airings share a poster ask
/// for it once, since two downloads of one URL would race.
#[tokio::test]
async fn a_poster_two_channels_share_is_pre_cached_once() {
    const SHARED: [Airing; 2] = [
        ("one.tv", "06", "07", "One Early", "http://img/series.png"),
        ("two.tv", "06", "07", "Two Early", "http://img/series.png"),
    ];
    let lab =
        item_lab_reading_through(M3U, Some(guide(&guide_day(0), &SHARED)), |inner| inner).await;
    let art = Arc::new(ArtServices::default());
    lab.mgr.set_images(art.clone(), art.clone());
    lab.mgr.refresh_guide().await.expect("refresh");

    assert_eq!(
        crate::guide_repository::test_support::stored_program_items(&lab.mgr.db)
            .await
            .expect("programme items")
            .len(),
        2
    );
    assert_eq!(
        *art.cached.lock().expect("cache log"),
        ["http://img/series.png"]
    );
}

/// gl-1591, gl-1503: `RefreshChannelsInternal` stores, on each channel item,
/// what its airings add up to (`GuideManager.cs:275-316`):
/// - `IsMovie` and `IsSeries`;
/// - a "Kids" tag (indexed, so `tags=Kids` finds the channel);
/// - and, from `GetChannel`, the `ExternalServiceId` provider row.
///
/// A second refresh of the same guide writes no channel item.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "three refreshes of one lineup, each checked against the last"
)]
async fn a_guide_refresh_stores_its_channels_aggregates_and_service_id() {
    use ferrofin_model::data::BaseItemKind;
    let day = guide_day(0);
    let xml = format!(
        "<tv>\
         <channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
         <channel id=\"two.tv\"><display-name>Channel Two</display-name></channel>\
         <programme start=\"{day}060000 +0000\" stop=\"{day}070000 +0000\" channel=\"one.tv\">\
         <title>Cartoon Hour</title><category>Kids</category>\
         <episode-num system=\"xmltv_ns\">0.1.</episode-num></programme>\
         <programme start=\"{day}060000 +0000\" stop=\"{day}080000 +0000\" channel=\"two.tv\">\
         <title>A Feature</title><category>Movie</category></programme>\
         </tv>"
    );
    let lab = item_lab_reading_through(M3U, Some(xml), |inner| inner).await;
    lab.mgr.refresh_guide().await.expect("refresh");

    let channels = |tags: Vec<String>| {
        let items = Arc::clone(&lab.items);
        async move {
            items
                .get_item_list(&InternalItemsQuery {
                    include_item_types: vec![BaseItemKind::LiveTvChannel],
                    tags,
                    ..InternalItemsQuery::default()
                })
                .await
                .expect("channel items")
                .into_iter()
                .map(|row| (row.name.clone().unwrap_or_default(), row))
                .collect::<std::collections::BTreeMap<_, _>>()
        }
    };
    let all = channels(Vec::new()).await;
    let (one, two) = (&all["Channel One"], &all["Channel Two"]);
    assert!(one.is_series && !one.is_movie, "{one:?}");
    assert_eq!(one.tags.as_deref(), Some("Kids"));
    assert!(two.is_movie && !two.is_series, "{two:?}");
    assert_eq!(two.tags, None, "no kids airing, no tag");
    assert_eq!(
        channels(vec!["Kids".to_owned()])
            .await
            .into_keys()
            .collect::<Vec<_>>(),
        ["Channel One"],
        "the tag is indexed"
    );
    let ids: Vec<Uuid> = [one, two]
        .iter()
        .map(|row| Uuid::parse_str(&row.id).expect("guid"))
        .collect();
    let providers = lab
        .persistence
        .provider_ids_for_items(&ids)
        .await
        .expect("provider ids");
    for id in &ids {
        assert_eq!(
            providers[id],
            [("ExternalServiceId".to_owned(), "Emby".to_owned())],
            "{id}"
        );
    }

    // Channel two as Jellyfin stores an item without tags (`Tags = ''`): no
    // tag value to index, so an unchanged guide writes none for it either.
    let mut untagged = two.clone();
    untagged.tags = Some(String::new());
    lab.persistence
        .save_items(&[untagged])
        .await
        .expect("an adopted, untagged channel");
    let all = channels(Vec::new()).await;
    let db = &lab.mgr.db;
    crate::guide_repository::test_support::count_program_writes(db)
        .await
        .expect("count writes");

    lab.mgr.refresh_guide().await.expect("second refresh");
    assert_eq!(
        crate::guide_repository::test_support::take_program_writes(db)
            .await
            .expect("writes"),
        (0, 0),
        "an unchanged guide writes no programme and no tag value"
    );
    let again = channels(Vec::new()).await;
    for name in ["Channel One", "Channel Two"] {
        assert_eq!(
            again[name].date_last_saved, all[name].date_last_saved,
            "{name}: an unchanged guide does not write the channel again"
        );
    }

    // A provider row naming another service is put right; another key stays.
    let two_id = ids[1];
    lab.persistence
        .save_provider_id(two_id, "ExternalServiceId", "Other")
        .await
        .expect("a stale service");
    lab.persistence
        .save_provider_id(two_id, "Custom", "kept")
        .await
        .expect("an editor's id");
    // The next guide: channel one airs only a sub-titled episode (no episode
    // number, so the listing's IsSeries is false) and channel two nothing.
    let xml = format!(
        "<tv>\
         <channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
         <channel id=\"two.tv\"><display-name>Channel Two</display-name></channel>\
         <programme start=\"{day}090000 +0000\" stop=\"{day}100000 +0000\" channel=\"one.tv\">\
         <title>Talk</title><sub-title>Guests</sub-title></programme>\
         </tv>"
    );
    lab.sources
        .0
        .lock()
        .expect("sources")
        .insert("http://guide/xmltv.xml".to_owned(), xml);
    let providers = lab.mgr.get_listing_providers().await.expect("providers");
    let provider = providers[0].id.as_deref().expect("provider id");
    tokio::fs::remove_file(lab.mgr.xmltv.cache_file_for_test(provider))
        .await
        .expect("drop the cached guide");
    lab.mgr.refresh_guide().await.expect("third refresh");
    let last = channels(Vec::new()).await;
    let (one, two) = (&last["Channel One"], &last["Channel Two"]);
    assert!(
        !one.is_series,
        "a sub-title alone is no series for the channel"
    );
    assert_eq!(
        one.tags.as_deref(),
        Some("Kids"),
        "the Kids tag is never removed"
    );
    assert!(
        !two.is_movie,
        "a channel whose airings left the guide is reset"
    );
    let mut provider_ids = lab
        .persistence
        .provider_ids_for_items(&[two_id])
        .await
        .expect("provider ids")
        .remove(&two_id)
        .expect("rows");
    provider_ids.sort();
    assert_eq!(
        provider_ids,
        [
            ("Custom".to_owned(), "kept".to_owned()),
            ("ExternalServiceId".to_owned(), "Emby".to_owned()),
        ]
    );
}
