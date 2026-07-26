//! The real [`LiveTvManager`] over the SQLite channel/guide cache.
//!
//! Configuration (tuner hosts, listing providers) is stored verbatim as JSON so
//! reads round-trip the DTO. `refresh_guide` fetches each tuner host (M3U) and
//! listing provider (XMLTV), rewrites `LiveTvChannels`/`LiveTvPrograms`, and
//! binds programmes to channels by the tuner `tvg-id` / XMLTV `channel id`.
//! Channels and programmes are surfaced to clients as `BaseItemDto`s.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hermit_db::Database;
use sqlx::Row;
use uuid::Uuid;

use hermit_model::data::{BaseItemKind, MediaType};
use hermit_model::dto::BaseItemDto;
use hermit_model::live_tv::{
    ChannelType, ListingsProviderInfo, LiveTvInfo, LiveTvServiceInfo, LiveTvServiceStatus,
    TunerHostInfo,
};
use hermit_model::querying::QueryResult;
use hermit_traits::error::ServiceError;
use hermit_traits::options::{DtoOptions, InternalItemsQuery};
use hermit_traits::stubs::LiveTvManager;

use crate::fetch::SourceFetcher;
use crate::m3u::parse_m3u;
use crate::xmltv::parse_xmltv;

/// Namespace for deriving stable channel UUIDs (v5) from `tuner-host|tvg-id`.
const CHANNEL_NS: Uuid = Uuid::from_u128(0x6c74_7663_6861_6e6e_656c_735f_6e73_3031);
/// Namespace for deriving stable programme UUIDs (v5) from `channel|start`.
const PROGRAM_NS: Uuid = Uuid::from_u128(0x6c74_7670_726f_6772_616d_735f_6e73_3031);

/// Concrete Live TV manager backed by [`Database`] and a [`SourceFetcher`].
#[derive(Clone)]
pub struct HermitLiveTvManager {
    db: Database,
    fetcher: Arc<dyn SourceFetcher>,
    server_id: String,
}

impl std::fmt::Debug for HermitLiveTvManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitLiveTvManager")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

impl HermitLiveTvManager {
    /// Creates the manager over the given database and source fetcher.
    #[must_use]
    pub fn new(db: Database, fetcher: Arc<dyn SourceFetcher>, server_id: String) -> Self {
        Self {
            db,
            fetcher,
            server_id,
        }
    }

    /// Rewrites the channel lineup for one tuner host from its M3U body, in a
    /// transaction (deleting the old channels cascades away their programmes).
    async fn replace_channels(&self, tuner_id: &str, m3u_body: &str) -> Result<(), ServiceError> {
        let channels = parse_m3u(m3u_body);
        let mut tx = self.db.pool().begin().await.map_err(db_err)?;

        sqlx::query(r#"DELETE FROM "LiveTvChannels" WHERE "TunerHostId" = ?1"#)
            .bind(tuner_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        for (index, ch) in channels.iter().enumerate() {
            let key = if ch.id.is_empty() { &ch.name } else { &ch.id };
            let id = Uuid::new_v5(&CHANNEL_NS, format!("{tuner_id}|{key}").as_bytes());
            let channel_type = if ch.is_radio { "Radio" } else { "Tv" };
            sqlx::query(
                r#"INSERT INTO "LiveTvChannels"
                   ("Id","TunerHostId","TvgId","Name","Number","ImageUrl","ChannelType","StreamUrl","SortIndex")
                   VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)"#,
            )
            .bind(id.to_string())
            .bind(tuner_id)
            .bind(&ch.id)
            .bind(&ch.name)
            .bind(&ch.number)
            .bind(&ch.logo)
            .bind(channel_type)
            .bind(&ch.url)
            .bind(i64::try_from(index).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)
    }

    /// Inserts programmes from an XMLTV body, binding each to every channel whose
    /// `TvgId` matches the programme's `channel` attribute.
    async fn insert_programs(&self, xmltv_body: &str) -> Result<(), ServiceError> {
        let guide = parse_xmltv(xmltv_body);

        // Map each tvg-id to the channel UUIDs that carry it.
        let rows = sqlx::query(r#"SELECT "Id","TvgId" FROM "LiveTvChannels""#)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        let mut by_tvg: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let id: String = row.get("Id");
            let tvg: String = row.get("TvgId");
            by_tvg.entry(tvg).or_default().push(id);
        }

        let mut tx = self.db.pool().begin().await.map_err(db_err)?;
        for prog in &guide.programmes {
            let Some(channel_ids) = by_tvg.get(&prog.channel_id) else {
                continue;
            };
            let start = prog.start.map(|s| s.to_rfc3339()).unwrap_or_default();
            let end = prog.stop.map(|s| s.to_rfc3339());
            let genres = if prog.categories.is_empty() {
                None
            } else {
                serde_json::to_string(&prog.categories).ok()
            };
            for channel_id in channel_ids {
                let id = Uuid::new_v5(&PROGRAM_NS, format!("{channel_id}|{start}").as_bytes());
                sqlx::query(
                    r#"INSERT OR REPLACE INTO "LiveTvPrograms"
                       ("Id","ChannelId","StartDate","EndDate","Title","EpisodeTitle","Overview",
                        "Genres","ImageUrl","ProductionYear","EpisodeNum","IsNew","IsPremiere",
                        "IsRepeat","OfficialRating")
                       VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"#,
                )
                .bind(id.to_string())
                .bind(channel_id)
                .bind(&start)
                .bind(&end)
                .bind(&prog.title)
                .bind(&prog.sub_title)
                .bind(&prog.desc)
                .bind(&genres)
                .bind(&prog.icon)
                .bind(prog.year)
                .bind(&prog.episode_num)
                .bind(i32::from(prog.is_new))
                .bind(i32::from(prog.is_premiere))
                .bind(i32::from(prog.is_previously_shown))
                .bind(&prog.rating)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
        }
        tx.commit().await.map_err(db_err)
    }
}

#[async_trait]
impl LiveTvManager for HermitLiveTvManager {
    async fn get_live_tv_info(&self) -> Result<LiveTvInfo, ServiceError> {
        let tuners = self.get_tuner_hosts().await?;
        let enabled = !tuners.is_empty();
        let services = if enabled {
            vec![LiveTvServiceInfo {
                name: Some("M3U/XMLTV".to_owned()),
                status: LiveTvServiceStatus::Ok,
                is_visible: true,
                ..LiveTvServiceInfo::default()
            }]
        } else {
            Vec::new()
        };
        Ok(LiveTvInfo {
            services,
            is_enabled: enabled,
            enabled_users: Vec::new(),
        })
    }

    async fn get_tuner_hosts(&self) -> Result<Vec<TunerHostInfo>, ServiceError> {
        let rows = sqlx::query(r#"SELECT "Data" FROM "LiveTvTunerHosts" ORDER BY "Id""#)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| serde_json::from_str(r.get::<String, _>("Data").as_str()).ok())
            .collect())
    }

    async fn save_tuner_host(
        &self,
        mut info: TunerHostInfo,
    ) -> Result<TunerHostInfo, ServiceError> {
        let id = info
            .id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        info.id = Some(id.clone());
        if info.type_.is_none() {
            info.type_ = Some("m3u".to_owned());
        }
        let url = info.url.clone().unwrap_or_default();
        if url.is_empty() {
            return Err(ServiceError::InvalidInput(
                "tuner host Url is required".into(),
            ));
        }
        let data = serde_json::to_string(&info)
            .map_err(|e| ServiceError::Backend(format!("serialize tuner host: {e}")))?;
        sqlx::query(
            r#"INSERT INTO "LiveTvTunerHosts" ("Id","Url","Type","Data") VALUES (?1,?2,?3,?4)
               ON CONFLICT("Id") DO UPDATE SET "Url"=excluded."Url","Type"=excluded."Type","Data"=excluded."Data""#,
        )
        .bind(&id)
        .bind(&url)
        .bind(info.type_.as_deref().unwrap_or("m3u"))
        .bind(&data)
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(info)
    }

    async fn delete_tuner_host(&self, id: &str) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "LiveTvTunerHosts" WHERE "Id" = ?1"#)
            .bind(id)
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn get_listing_providers(&self) -> Result<Vec<ListingsProviderInfo>, ServiceError> {
        let rows = sqlx::query(r#"SELECT "Data" FROM "LiveTvListingProviders" ORDER BY "Id""#)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| serde_json::from_str(r.get::<String, _>("Data").as_str()).ok())
            .collect())
    }

    async fn save_listing_provider(
        &self,
        mut info: ListingsProviderInfo,
    ) -> Result<ListingsProviderInfo, ServiceError> {
        let id = info
            .id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        info.id = Some(id.clone());
        if info.type_.is_none() {
            info.type_ = Some("xmltv".to_owned());
        }
        let path = info.path.clone().unwrap_or_default();
        if path.is_empty() {
            return Err(ServiceError::InvalidInput(
                "listing provider Path is required".into(),
            ));
        }
        let data = serde_json::to_string(&info)
            .map_err(|e| ServiceError::Backend(format!("serialize listing provider: {e}")))?;
        sqlx::query(
            r#"INSERT INTO "LiveTvListingProviders" ("Id","Type","Path","Data") VALUES (?1,?2,?3,?4)
               ON CONFLICT("Id") DO UPDATE SET "Type"=excluded."Type","Path"=excluded."Path","Data"=excluded."Data""#,
        )
        .bind(&id)
        .bind(info.type_.as_deref().unwrap_or("xmltv"))
        .bind(&path)
        .bind(&data)
        .execute(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(info)
    }

    async fn delete_listing_provider(&self, id: &str) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "LiveTvListingProviders" WHERE "Id" = ?1"#)
            .bind(id)
            .execute(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn get_channels(
        &self,
        _options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        let rows = sqlx::query(
            r#"SELECT "Id","Name","Number","ChannelType","ImageUrl"
               FROM "LiveTvChannels" ORDER BY "SortIndex", "Name""#,
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let items: Vec<BaseItemDto> = rows.iter().map(|r| self.channel_dto(r)).collect();
        Ok(QueryResult::from_items(items))
    }

    async fn get_channel(
        &self,
        id: Uuid,
        _options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError> {
        let row = sqlx::query(
            r#"SELECT "Id","Name","Number","ChannelType","ImageUrl"
               FROM "LiveTvChannels" WHERE "Id" = ?1"#,
        )
        .bind(id.to_string())
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(row.map(|r| self.channel_dto(&r)))
    }

    async fn get_programs(
        &self,
        query: &InternalItemsQuery,
        _options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        // Optional channel filter drawn from the query's channel ids.
        let channel_filter: Vec<String> =
            query.channel_ids.iter().map(ToString::to_string).collect();
        let rows = sqlx::query(
            r#"SELECT p."Id",p."ChannelId",p."StartDate",p."EndDate",p."Title",p."EpisodeTitle",
                      p."Overview",p."Genres",p."ProductionYear",p."OfficialRating",p."IsNew",
                      p."IsRepeat",p."IsPremiere",c."Name" AS "ChannelName"
               FROM "LiveTvPrograms" p
               JOIN "LiveTvChannels" c ON c."Id" = p."ChannelId"
               ORDER BY p."StartDate""#,
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let items: Vec<BaseItemDto> = rows
            .iter()
            .filter(|r| {
                channel_filter.is_empty()
                    || channel_filter.contains(&r.get::<String, _>("ChannelId"))
            })
            .map(|r| self.program_dto(r))
            .collect();
        Ok(QueryResult::from_items(items))
    }

    async fn get_program(
        &self,
        id: Uuid,
        _options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError> {
        let row = sqlx::query(
            r#"SELECT p."Id",p."ChannelId",p."StartDate",p."EndDate",p."Title",p."EpisodeTitle",
                      p."Overview",p."Genres",p."ProductionYear",p."OfficialRating",p."IsNew",
                      p."IsRepeat",p."IsPremiere",c."Name" AS "ChannelName"
               FROM "LiveTvPrograms" p
               JOIN "LiveTvChannels" c ON c."Id" = p."ChannelId"
               WHERE p."Id" = ?1"#,
        )
        .bind(id.to_string())
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(row.map(|r| self.program_dto(&r)))
    }

    async fn reset_tuner(&self, _id: &str) -> Result<(), ServiceError> {
        // M3U tuners are stateless HTTP streams — there is nothing to reset.
        Ok(())
    }

    async fn refresh_guide(&self) -> Result<(), ServiceError> {
        for tuner in self.get_tuner_hosts().await? {
            let (Some(id), Some(url)) = (tuner.id.as_deref(), tuner.url.as_deref()) else {
                continue;
            };
            match self.fetcher.fetch(url).await {
                Ok(body) => self.replace_channels(id, &body).await?,
                Err(e) => tracing::warn!(%url, error = %e, "live tv: tuner fetch failed"),
            }
        }
        for provider in self.get_listing_providers().await? {
            let Some(path) = provider.path.as_deref() else {
                continue;
            };
            match self.fetcher.fetch(path).await {
                Ok(body) => self.insert_programs(&body).await?,
                Err(e) => tracing::warn!(%path, error = %e, "live tv: guide fetch failed"),
            }
        }
        Ok(())
    }

    async fn get_channel_stream_url(&self, id: Uuid) -> Result<Option<String>, ServiceError> {
        let url: Option<String> =
            sqlx::query_scalar(r#"SELECT "StreamUrl" FROM "LiveTvChannels" WHERE "Id" = ?1"#)
                .bind(id.to_string())
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(url)
    }
}

impl HermitLiveTvManager {
    /// Maps a channel row to a `BaseItemDto` (`Type = "TvChannel"`).
    fn channel_dto(&self, r: &sqlx::sqlite::SqliteRow) -> BaseItemDto {
        let channel_type = match r.get::<String, _>("ChannelType").as_str() {
            "Radio" => ChannelType::Radio,
            _ => ChannelType::Tv,
        };
        let media_type = if channel_type == ChannelType::Radio {
            MediaType::Audio
        } else {
            MediaType::Video
        };
        let id = Uuid::parse_str(&r.get::<String, _>("Id")).unwrap_or_default();
        BaseItemDto {
            id,
            server_id: Some(self.server_id.clone()),
            name: Some(r.get::<String, _>("Name")),
            type_: BaseItemKind::TvChannel,
            channel_type: Some(channel_type),
            media_type,
            number: r.get::<Option<String>, _>("Number").clone(),
            channel_number: r.get::<Option<String>, _>("Number"),
            is_folder: Some(false),
            ..BaseItemDto::default()
        }
    }

    /// Maps a program row (joined to its channel) to a `BaseItemDto`
    /// (`Type = "LiveTvProgram"`).
    fn program_dto(&self, r: &sqlx::sqlite::SqliteRow) -> BaseItemDto {
        let id = Uuid::parse_str(&r.get::<String, _>("Id")).unwrap_or_default();
        let channel_id = Uuid::parse_str(&r.get::<String, _>("ChannelId")).ok();
        let genres: Option<Vec<String>> = r
            .get::<Option<String>, _>("Genres")
            .and_then(|g| serde_json::from_str(&g).ok());
        BaseItemDto {
            id,
            server_id: Some(self.server_id.clone()),
            name: Some(r.get::<String, _>("Title")),
            type_: BaseItemKind::LiveTvProgram,
            channel_id,
            media_type: MediaType::Unknown,
            episode_title: r.get::<Option<String>, _>("EpisodeTitle"),
            overview: r.get::<Option<String>, _>("Overview"),
            genres,
            production_year: r.get::<Option<i32>, _>("ProductionYear"),
            official_rating: r.get::<Option<String>, _>("OfficialRating"),
            start_date: parse_dt(r.get::<String, _>("StartDate").as_str()),
            end_date: r
                .get::<Option<String>, _>("EndDate")
                .as_deref()
                .and_then(parse_dt),
            channel_name: r.get::<Option<String>, _>("ChannelName"),
            is_folder: Some(false),
            ..BaseItemDto::default()
        }
    }
}

/// Parses an RFC-3339 timestamp stored in the guide cache.
fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Maps a `sqlx` error into a [`ServiceError`] via `hermit-db`'s `DbError`, for
/// consistency with the repository layer's error text.
fn db_err(e: sqlx::Error) -> ServiceError {
    ServiceError::from(hermit_db::DbError::from(e))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use hermit_db::Database;
    use hermit_model::live_tv::{ListingsProviderInfo, TunerHostInfo};
    use hermit_traits::options::{DtoOptions, InternalItemsQuery};
    use hermit_traits::stubs::LiveTvManager;

    use super::{HermitLiveTvManager, SourceFetcher};

    /// An in-memory [`SourceFetcher`] mapping URL → body for offline tests.
    struct FakeFetcher(HashMap<String, String>);

    #[async_trait::async_trait]
    impl SourceFetcher for FakeFetcher {
        async fn fetch(&self, url: &str) -> Result<String, hermit_traits::error::ServiceError> {
            self.0
                .get(url)
                .cloned()
                .ok_or_else(|| hermit_traits::error::ServiceError::Backend(format!("no {url}")))
        }
    }

    async fn manager_with(fetcher: FakeFetcher) -> HermitLiveTvManager {
        let db = Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrate");
        HermitLiveTvManager::new(db, std::sync::Arc::new(fetcher), "srv".to_owned())
    }

    const M3U: &str = "#EXTM3U\n\
        #EXTINF:-1 tvg-id=\"one.tv\" tvg-chno=\"1\",Channel One\nhttp://tuner/one\n\
        #EXTINF:-1 tvg-id=\"two.tv\" tvg-chno=\"2\",Channel Two\nhttp://tuner/two\n";
    const XMLTV: &str = "<tv>\
        <channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
        <programme start=\"20260725060000 +0000\" stop=\"20260725070000 +0000\" channel=\"one.tv\">\
        <title>Morning Show</title><desc>News.</desc><category>News</category></programme>\
        </tv>";

    #[tokio::test]
    async fn tuner_host_crud_roundtrips() {
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let saved = mgr
            .save_tuner_host(TunerHostInfo {
                url: Some("http://tuner/playlist.m3u".to_owned()),
                ..TunerHostInfo::default()
            })
            .await
            .expect("save");
        let id = saved.id.clone().expect("id assigned");
        assert_eq!(saved.type_.as_deref(), Some("m3u"));

        let hosts = mgr.get_tuner_hosts().await.expect("list");
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].url.as_deref(), Some("http://tuner/playlist.m3u"));

        mgr.delete_tuner_host(&id).await.expect("delete");
        assert!(mgr.get_tuner_hosts().await.expect("list2").is_empty());
    }

    #[tokio::test]
    async fn tuner_host_without_url_is_rejected() {
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let err = mgr
            .save_tuner_host(TunerHostInfo::default())
            .await
            .expect_err("no url");
        assert!(matches!(
            err,
            hermit_traits::error::ServiceError::InvalidInput(_)
        ));
    }

    #[tokio::test]
    async fn listing_provider_crud_roundtrips() {
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let saved = mgr
            .save_listing_provider(ListingsProviderInfo {
                path: Some("http://guide/xmltv.xml".to_owned()),
                ..ListingsProviderInfo::default()
            })
            .await
            .expect("save");
        assert_eq!(saved.type_.as_deref(), Some("xmltv"));
        let id = saved.id.clone().expect("id");
        assert_eq!(mgr.get_listing_providers().await.expect("list").len(), 1);
        mgr.delete_listing_provider(&id).await.expect("delete");
        assert!(mgr.get_listing_providers().await.expect("list2").is_empty());
    }

    #[tokio::test]
    async fn refresh_populates_channels_and_guide() {
        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), M3U.to_owned());
        sources.insert("http://guide/xmltv.xml".to_owned(), XMLTV.to_owned());
        let mgr = manager_with(FakeFetcher(sources)).await;

        mgr.save_tuner_host(TunerHostInfo {
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        mgr.save_listing_provider(ListingsProviderInfo {
            path: Some("http://guide/xmltv.xml".to_owned()),
            ..ListingsProviderInfo::default()
        })
        .await
        .expect("provider");

        mgr.refresh_guide().await.expect("refresh");

        // Info reports enabled once a tuner is configured.
        assert!(mgr.get_live_tv_info().await.expect("info").is_enabled);

        let channels = mgr
            .get_channels(&DtoOptions::default())
            .await
            .expect("chans");
        assert_eq!(channels.total_record_count, 2);
        assert_eq!(channels.items[0].name.as_deref(), Some("Channel One"));
        assert_eq!(channels.items[0].channel_number.as_deref(), Some("1"));

        let chan_id = channels.items[0].id;
        let stream = mgr
            .get_channel_stream_url(chan_id)
            .await
            .expect("stream")
            .expect("url");
        assert_eq!(stream, "http://tuner/one");

        // The guide programme binds to Channel One (tvg-id one.tv) only.
        let programs = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("progs");
        assert_eq!(programs.total_record_count, 1);
        assert_eq!(programs.items[0].name.as_deref(), Some("Morning Show"));
        assert_eq!(programs.items[0].channel_id, Some(chan_id));

        // Refresh is idempotent: stable ids mean the counts don't grow.
        mgr.refresh_guide().await.expect("refresh2");
        assert_eq!(
            mgr.get_channels(&DtoOptions::default())
                .await
                .expect("c2")
                .total_record_count,
            2
        );
        assert_eq!(
            mgr.get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
                .await
                .expect("p2")
                .total_record_count,
            1
        );

        // Deleting the tuner host cascades its channels (and their programmes).
        let host_id = mgr
            .get_tuner_hosts()
            .await
            .expect("h")
            .pop()
            .unwrap()
            .id
            .unwrap();
        mgr.delete_tuner_host(&host_id).await.expect("del host");
        assert_eq!(
            mgr.get_channels(&DtoOptions::default())
                .await
                .expect("c3")
                .total_record_count,
            0
        );
    }
}
