//! The real [`LiveTvManager`] over the SQLite channel/guide cache.
//!
//! Configuration (tuner hosts, listing providers) is stored verbatim as JSON so
//! reads round-trip the DTO. `refresh_guide` fetches each tuner host (M3U) and
//! listing provider (XMLTV), rewrites `HermitLiveTvChannels`/`HermitLiveTvPrograms`, and
//! binds programmes to channels by the tuner `tvg-id` / XMLTV `channel id`.
//! Channels and programmes are surfaced to clients as `BaseItemDto`s.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hermit_db::Database;
use hermit_db::store::{datetime_to_db, guid_to_db, opt_datetime_to_db};
use sqlx::{QueryBuilder, Row, Sqlite};
use uuid::Uuid;

use hermit_model::data::{BaseItemKind, MediaType};
use hermit_model::dto::BaseItemDto;
use hermit_model::live_tv::{
    ChannelType, ListingsProviderInfo, LiveTvInfo, LiveTvServiceInfo, LiveTvServiceStatus,
    RecordingStatus, SeriesTimerInfoDto, TimerInfoDto, TunerHostInfo,
};
use hermit_model::querying::QueryResult;
use hermit_traits::error::ServiceError;
use hermit_traits::options::{DtoOptions, InternalItemsQuery};
use hermit_traits::stubs::LiveTvManager;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::LiveTvError;
use crate::fetch::SourceFetcher;
use crate::m3u::parse_m3u;
use crate::xmltv::parse_xmltv;

/// SQLite's conservative default bind-parameter limit (`SQLITE_MAX_VARIABLE_NUMBER`
/// is 999 before 3.32, 32766 after); multi-row inserts chunk to stay under it.
const SQLITE_BIND_LIMIT: usize = 999;

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
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;

        sqlx::query(r#"DELETE FROM "HermitLiveTvChannels" WHERE "TunerHostId" = ?1"#)
            .bind(tuner_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        // 9 columns per row; chunked multi-row insert instead of one round-trip
        // per channel.
        for (chunk_index, chunk) in channels.chunks(SQLITE_BIND_LIMIT / 9).enumerate() {
            let mut qb: QueryBuilder<'_, Sqlite> = QueryBuilder::new(
                r#"INSERT INTO "HermitLiveTvChannels"
                   ("Id","TunerHostId","TvgId","Name","Number","ImageUrl","ChannelType","StreamUrl","SortIndex") "#,
            );
            let base = chunk_index * (SQLITE_BIND_LIMIT / 9);
            qb.push_values(chunk.iter().enumerate(), |mut b, (offset, ch)| {
                let key = if ch.id.is_empty() { &ch.name } else { &ch.id };
                let id = Uuid::new_v5(&CHANNEL_NS, format!("{tuner_id}|{key}").as_bytes());
                let channel_type = if ch.is_radio { "Radio" } else { "Tv" };
                b.push_bind(guid_to_db(id))
                    .push_bind(tuner_id)
                    .push_bind(&ch.id)
                    .push_bind(&ch.name)
                    .push_bind(&ch.number)
                    .push_bind(&ch.logo)
                    .push_bind(channel_type)
                    .push_bind(&ch.url)
                    .push_bind(i64::try_from(base + offset).unwrap_or(i64::MAX));
            });
            qb.build().execute(&mut *tx).await.map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)
    }

    /// Inserts programmes from an XMLTV body, binding each to every channel whose
    /// `TvgId` matches the programme's `channel` attribute.
    async fn insert_programs(&self, xmltv_body: &str) -> Result<(), ServiceError> {
        let guide = parse_xmltv(xmltv_body);

        // Map each tvg-id to the channel UUIDs that carry it.
        let rows = sqlx::query(r#"SELECT "Id","TvgId" FROM "HermitLiveTvChannels""#)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        let mut by_tvg: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let id: String = row.get("Id");
            let tvg: String = row.get("TvgId");
            by_tvg.entry(tvg).or_default().push(id);
        }

        // Flatten to one (channel, programme) row per binding, then insert in
        // chunked multi-row statements (15 columns per row) instead of one
        // round-trip per programme.
        let rows: Vec<_> = guide
            .programmes
            .iter()
            .flat_map(|prog| {
                let channel_ids = by_tvg.get(&prog.channel_id).map_or(&[][..], Vec::as_slice);
                let start = opt_datetime_to_db(prog.start).unwrap_or_default();
                let end = opt_datetime_to_db(prog.stop);
                let genres = if prog.categories.is_empty() {
                    None
                } else {
                    serde_json::to_string(&prog.categories).ok()
                };
                channel_ids.iter().map(move |channel_id| {
                    let id = Uuid::new_v5(&PROGRAM_NS, format!("{channel_id}|{start}").as_bytes());
                    (
                        guid_to_db(id),
                        channel_id,
                        start.clone(),
                        end.clone(),
                        genres.clone(),
                        prog,
                    )
                })
            })
            .collect();

        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        for chunk in rows.chunks(SQLITE_BIND_LIMIT / 15) {
            let mut qb: QueryBuilder<'_, Sqlite> = QueryBuilder::new(
                r#"INSERT OR REPLACE INTO "HermitLiveTvPrograms"
                   ("Id","ChannelId","StartDate","EndDate","Title","EpisodeTitle","Overview",
                    "Genres","ImageUrl","ProductionYear","EpisodeNum","IsNew","IsPremiere",
                    "IsRepeat","OfficialRating") "#,
            );
            qb.push_values(
                chunk,
                |mut b, (id, channel_id, start, end, genres, prog)| {
                    b.push_bind(id)
                        .push_bind(*channel_id)
                        .push_bind(start)
                        .push_bind(end)
                        .push_bind(&prog.title)
                        .push_bind(&prog.sub_title)
                        .push_bind(&prog.desc)
                        .push_bind(genres)
                        .push_bind(&prog.icon)
                        .push_bind(prog.year)
                        .push_bind(&prog.episode_num)
                        .push_bind(i32::from(prog.is_new))
                        .push_bind(i32::from(prog.is_premiere))
                        .push_bind(i32::from(prog.is_previously_shown))
                        .push_bind(&prog.rating);
                },
            );
            qb.build().execute(&mut *tx).await.map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }
}

#[async_trait]
impl LiveTvManager for HermitLiveTvManager {
    async fn get_live_tv_info(&self) -> Result<LiveTvInfo, ServiceError> {
        // Always emit the built-in "Emby" service, mirroring Jellyfin's
        // DefaultLiveTvService (which is always registered), then optionally
        // append the M3U/XMLTV entry once a tuner host is configured.
        // Jellyfin's DefaultLiveTvService reports IsVisible=false and an (empty) Tuners array.
        let mut services = vec![LiveTvServiceInfo {
            name: Some("Emby".to_owned()),
            status: LiveTvServiceStatus::Ok,
            is_visible: false,
            tuners: Some(Vec::new()),
            ..LiveTvServiceInfo::default()
        }];
        if !self.get_tuner_hosts().await?.is_empty() {
            services.push(LiveTvServiceInfo {
                name: Some("M3U/XMLTV".to_owned()),
                status: LiveTvServiceStatus::Ok,
                is_visible: true,
                ..LiveTvServiceInfo::default()
            });
        }
        Ok(LiveTvInfo {
            is_enabled: !services.is_empty(),
            services,
            enabled_users: Vec::new(),
        })
    }

    async fn get_tuner_hosts(&self) -> Result<Vec<TunerHostInfo>, ServiceError> {
        let rows = sqlx::query(r#"SELECT "Data" FROM "HermitLiveTvTunerHosts" ORDER BY "Id""#)
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
            .unwrap_or_else(|| guid_to_db(Uuid::new_v4()));
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
            .map_err(|e| LiveTvError::serialize("serialize tuner host", e))?;
        sqlx::query(
            r#"INSERT INTO "HermitLiveTvTunerHosts" ("Id","Url","Type","Data") VALUES (?1,?2,?3,?4)
               ON CONFLICT("Id") DO UPDATE SET "Url"=excluded."Url","Type"=excluded."Type","Data"=excluded."Data""#,
        )
        .bind(&id)
        .bind(&url)
        .bind(info.type_.as_deref().unwrap_or("m3u"))
        .bind(&data)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(info)
    }

    async fn delete_tuner_host(&self, id: &str) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "HermitLiveTvTunerHosts" WHERE "Id" = ?1"#)
            .bind(id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn get_listing_providers(&self) -> Result<Vec<ListingsProviderInfo>, ServiceError> {
        let rows =
            sqlx::query(r#"SELECT "Data" FROM "HermitLiveTvListingProviders" ORDER BY "Id""#)
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
            .unwrap_or_else(|| guid_to_db(Uuid::new_v4()));
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
            .map_err(|e| LiveTvError::serialize("serialize listing provider", e))?;
        sqlx::query(
            r#"INSERT INTO "HermitLiveTvListingProviders" ("Id","Type","Path","Data") VALUES (?1,?2,?3,?4)
               ON CONFLICT("Id") DO UPDATE SET "Type"=excluded."Type","Path"=excluded."Path","Data"=excluded."Data""#,
        )
        .bind(&id)
        .bind(info.type_.as_deref().unwrap_or("xmltv"))
        .bind(&path)
        .bind(&data)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(info)
    }

    async fn delete_listing_provider(&self, id: &str) -> Result<(), ServiceError> {
        sqlx::query(r#"DELETE FROM "HermitLiveTvListingProviders" WHERE "Id" = ?1"#)
            .bind(id)
            .execute(self.db.writer())
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
               FROM "HermitLiveTvChannels" ORDER BY "SortIndex", "Name""#,
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
               FROM "HermitLiveTvChannels" WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
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
        // Optional channel filter drawn from the query's channel ids, in the
        // canonical stored GUID form so it compares equal to the "ChannelId"
        // column text.
        let channel_filter: Vec<String> =
            query.channel_ids.iter().copied().map(guid_to_db).collect();
        let rows = sqlx::query(
            r#"SELECT p."Id",p."ChannelId",p."StartDate",p."EndDate",p."Title",p."EpisodeTitle",
                      p."Overview",p."Genres",p."ProductionYear",p."OfficialRating",p."IsNew",
                      p."IsRepeat",p."IsPremiere",c."Name" AS "ChannelName"
               FROM "HermitLiveTvPrograms" p
               JOIN "HermitLiveTvChannels" c ON c."Id" = p."ChannelId"
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
               FROM "HermitLiveTvPrograms" p
               JOIN "HermitLiveTvChannels" c ON c."Id" = p."ChannelId"
               WHERE p."Id" = ?1"#,
        )
        .bind(guid_to_db(id))
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
            sqlx::query_scalar(r#"SELECT "StreamUrl" FROM "HermitLiveTvChannels" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(url)
    }

    async fn get_timers(&self) -> Result<Vec<TimerInfoDto>, ServiceError> {
        self.json_list(r#"SELECT "Data" FROM "HermitLiveTvTimers" ORDER BY "StartDate""#)
            .await
    }

    async fn get_timer(&self, id: &str) -> Result<Option<TimerInfoDto>, ServiceError> {
        self.json_get(
            r#"SELECT "Data" FROM "HermitLiveTvTimers" WHERE "Id" = ?1"#,
            id,
        )
        .await
    }

    async fn create_timer(&self, mut timer: TimerInfoDto) -> Result<String, ServiceError> {
        let id = ensure_id(&mut timer.base.id);
        let data = to_json(&timer)?;
        sqlx::query(
            r#"INSERT INTO "HermitLiveTvTimers"
               ("Id","ChannelId","ProgramId","SeriesTimerId","Name","StartDate","EndDate","Status",
                "PrePaddingSeconds","PostPaddingSeconds","Data")
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
               ON CONFLICT("Id") DO UPDATE SET
                 "ChannelId"=excluded."ChannelId","ProgramId"=excluded."ProgramId",
                 "SeriesTimerId"=excluded."SeriesTimerId","Name"=excluded."Name",
                 "StartDate"=excluded."StartDate","EndDate"=excluded."EndDate",
                 "Status"=excluded."Status","Data"=excluded."Data""#,
        )
        .bind(&id)
        .bind(guid_to_db(timer.base.channel_id))
        .bind(&timer.base.program_id)
        .bind(&timer.series_timer_id)
        .bind(timer.base.name.clone().unwrap_or_default())
        .bind(datetime_to_db(timer.base.start_date))
        .bind(datetime_to_db(timer.base.end_date))
        .bind(recording_status_name(timer.status))
        .bind(timer.base.pre_padding_seconds)
        .bind(timer.base.post_padding_seconds)
        .bind(&data)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(id)
    }

    async fn update_timer(&self, id: &str, mut timer: TimerInfoDto) -> Result<(), ServiceError> {
        timer.base.id = Some(id.to_owned());
        self.create_timer(timer).await.map(|_| ())
    }

    async fn cancel_timer(&self, id: &str) -> Result<(), ServiceError> {
        self.delete_by_id(r#"DELETE FROM "HermitLiveTvTimers" WHERE "Id" = ?1"#, id)
            .await
    }

    async fn get_series_timers(&self) -> Result<Vec<SeriesTimerInfoDto>, ServiceError> {
        self.json_list(r#"SELECT "Data" FROM "HermitLiveTvSeriesTimers" ORDER BY "Name""#)
            .await
    }

    async fn get_series_timer(&self, id: &str) -> Result<Option<SeriesTimerInfoDto>, ServiceError> {
        self.json_get(
            r#"SELECT "Data" FROM "HermitLiveTvSeriesTimers" WHERE "Id" = ?1"#,
            id,
        )
        .await
    }

    async fn create_series_timer(
        &self,
        mut timer: SeriesTimerInfoDto,
    ) -> Result<String, ServiceError> {
        let id = ensure_id(&mut timer.base.id);
        let data = to_json(&timer)?;
        sqlx::query(
            r#"INSERT INTO "HermitLiveTvSeriesTimers" ("Id","ChannelId","ProgramId","Name","Data")
               VALUES (?1,?2,?3,?4,?5)
               ON CONFLICT("Id") DO UPDATE SET
                 "ChannelId"=excluded."ChannelId","ProgramId"=excluded."ProgramId",
                 "Name"=excluded."Name","Data"=excluded."Data""#,
        )
        .bind(&id)
        .bind(guid_to_db(timer.base.channel_id))
        .bind(&timer.base.program_id)
        .bind(timer.base.name.clone().unwrap_or_default())
        .bind(&data)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(id)
    }

    async fn update_series_timer(
        &self,
        id: &str,
        mut timer: SeriesTimerInfoDto,
    ) -> Result<(), ServiceError> {
        timer.base.id = Some(id.to_owned());
        self.create_series_timer(timer).await.map(|_| ())
    }

    async fn cancel_series_timer(&self, id: &str) -> Result<(), ServiceError> {
        // Drop the series timer and any timers it scheduled.
        sqlx::query(r#"DELETE FROM "HermitLiveTvTimers" WHERE "SeriesTimerId" = ?1"#)
            .bind(id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        self.delete_by_id(
            r#"DELETE FROM "HermitLiveTvSeriesTimers" WHERE "Id" = ?1"#,
            id,
        )
        .await
    }

    async fn get_recordings(&self) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        let rows = sqlx::query(
            r#"SELECT "Id","Name","Overview","StartDate","EndDate","Status","ChannelId"
               FROM "HermitLiveTvRecordings" ORDER BY "StartDate" DESC"#,
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let items: Vec<BaseItemDto> = rows.iter().map(|r| self.recording_dto(r)).collect();
        Ok(QueryResult::from_items(items))
    }

    async fn get_recording(&self, id: Uuid) -> Result<Option<BaseItemDto>, ServiceError> {
        let row = sqlx::query(
            r#"SELECT "Id","Name","Overview","StartDate","EndDate","Status","ChannelId"
               FROM "HermitLiveTvRecordings" WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .fetch_optional(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(row.map(|r| self.recording_dto(&r)))
    }

    async fn get_recording_path(&self, id: Uuid) -> Result<Option<String>, ServiceError> {
        let path: Option<String> =
            sqlx::query_scalar(r#"SELECT "Path" FROM "HermitLiveTvRecordings" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?
                .flatten();
        // Only report a path that actually points at a captured file.
        Ok(path.filter(|p| !p.is_empty()))
    }

    async fn delete_recording(&self, id: Uuid) -> Result<(), ServiceError> {
        // Remove the file first (best-effort), then the row.
        let path: Option<String> =
            sqlx::query_scalar(r#"SELECT "Path" FROM "HermitLiveTvRecordings" WHERE "Id" = ?1"#)
                .bind(guid_to_db(id))
                .fetch_optional(self.db.pool())
                .await
                .map_err(db_err)?
                .flatten();
        if let Some(path) = path {
            let _ = tokio::fs::remove_file(&path).await;
        }
        self.delete_by_id(
            r#"DELETE FROM "HermitLiveTvRecordings" WHERE "Id" = ?1"#,
            &guid_to_db(id),
        )
        .await
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

    /// Maps a recording row to a `BaseItemDto` (`Type = "Recording"`).
    fn recording_dto(&self, r: &sqlx::sqlite::SqliteRow) -> BaseItemDto {
        let id = Uuid::parse_str(&r.get::<String, _>("Id")).unwrap_or_default();
        BaseItemDto {
            id,
            server_id: Some(self.server_id.clone()),
            name: Some(r.get::<String, _>("Name")),
            type_: BaseItemKind::Recording,
            channel_id: Uuid::parse_str(&r.get::<String, _>("ChannelId")).ok(),
            media_type: MediaType::Video,
            overview: r.get::<Option<String>, _>("Overview"),
            start_date: parse_dt(r.get::<String, _>("StartDate").as_str()),
            end_date: r
                .get::<Option<String>, _>("EndDate")
                .as_deref()
                .and_then(parse_dt),
            status: r.get::<Option<String>, _>("Status"),
            is_folder: Some(false),
            ..BaseItemDto::default()
        }
    }

    /// Reads a JSON `Data` column across all rows of `sql`, deserializing each.
    async fn json_list<T: DeserializeOwned>(&self, sql: &str) -> Result<Vec<T>, ServiceError> {
        let rows = sqlx::query(sql)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| serde_json::from_str(r.get::<String, _>("Data").as_str()).ok())
            .collect())
    }

    /// Reads and deserializes a single JSON `Data` column by id.
    async fn json_get<T: DeserializeOwned>(
        &self,
        sql: &str,
        id: &str,
    ) -> Result<Option<T>, ServiceError> {
        let data: Option<String> = sqlx::query_scalar(sql)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(data.and_then(|d| serde_json::from_str(&d).ok()))
    }

    /// Runs a `DELETE … WHERE "Id" = ?1` statement.
    async fn delete_by_id(&self, sql: &str, id: &str) -> Result<(), ServiceError> {
        sqlx::query(sql)
            .bind(id)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

/// Ensures a DTO id field is set, generating a fresh UUID when absent, and
/// returns it.
fn ensure_id(id: &mut Option<String>) -> String {
    let value = id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| guid_to_db(Uuid::new_v4()));
    *id = Some(value.clone());
    value
}

/// Serializes a DVR DTO to its stored JSON.
fn to_json<T: Serialize>(value: &T) -> Result<String, ServiceError> {
    serde_json::to_string(value).map_err(|e| LiveTvError::serialize("serialize timer", e).into())
}

/// The stored `Status` string for a [`RecordingStatus`].
fn recording_status_name(status: RecordingStatus) -> &'static str {
    match status {
        RecordingStatus::New => "New",
        RecordingStatus::InProgress => "InProgress",
        RecordingStatus::Completed => "Completed",
        RecordingStatus::Cancelled => "Cancelled",
        RecordingStatus::ConflictedOk => "ConflictedOk",
        RecordingStatus::ConflictedNotOk => "ConflictedNotOk",
        RecordingStatus::Error => "Error",
    }
}

/// Parses a timestamp stored in the guide cache: the canonical storage format
/// (`YYYY-MM-DD HH:MM:SS.fffffff`, UTC by convention — see
/// [`hermit_db::store`]), falling back to RFC-3339 for rows written before the
/// cache switched to the canonical format.
fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .map(|naive| naive.and_utc())
        .or_else(|| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        })
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

    use super::{HermitLiveTvManager, SourceFetcher, parse_dt};

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
    async fn info_always_has_emby_service_and_is_enabled() {
        // No tuner host configured: the built-in "Emby" service is still
        // present and IsEnabled is true, mirroring DefaultLiveTvService.
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let info = mgr.get_live_tv_info().await.expect("info");
        assert!(info.is_enabled);
        assert_eq!(info.services.len(), 1);
        assert_eq!(info.services[0].name.as_deref(), Some("Emby"));

        // Once a tuner exists, the M3U/XMLTV service is appended alongside Emby.
        mgr.save_tuner_host(TunerHostInfo {
            url: Some("http://tuner/playlist.m3u".to_owned()),
            ..TunerHostInfo::default()
        })
        .await
        .expect("tuner");
        let info = mgr.get_live_tv_info().await.expect("info2");
        assert!(info.is_enabled);
        assert_eq!(info.services.len(), 2);
        assert_eq!(info.services[0].name.as_deref(), Some("Emby"));
        assert_eq!(info.services[1].name.as_deref(), Some("M3U/XMLTV"));
    }

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

    #[tokio::test]
    async fn bulk_guide_sync_inserts_every_channel_and_program() {
        // 150 channels and 5000 programmes exceed a single insert chunk in both
        // paths, so this exercises the chunk boundaries and asserts no rows are
        // lost. It also prints the sync wall-time for before/after comparison.
        use std::fmt::Write as _;
        let mut m3u = String::from("#EXTM3U\n");
        for c in 0..150 {
            let _ = write!(
                m3u,
                "#EXTINF:-1 tvg-id=\"ch{c}.tv\" tvg-chno=\"{c}\",Channel {c}\nhttp://tuner/{c}\n"
            );
        }
        let mut xmltv = String::from("<tv>");
        for p in 0..5000u32 {
            let ch = p % 150;
            let day = 20 + p / 24 / 60 % 8;
            let hh = p / 60 % 24;
            let mm = p % 60;
            let _ = write!(
                xmltv,
                "<programme start=\"202607{day:02}{hh:02}{mm:02}00 +0000\" \
                 stop=\"202607{day:02}{hh:02}{mm:02}30 +0000\" channel=\"ch{ch}.tv\">\
                 <title>Show {p}</title></programme>"
            );
        }
        xmltv.push_str("</tv>");

        let mut sources = HashMap::new();
        sources.insert("http://tuner/playlist.m3u".to_owned(), m3u);
        sources.insert("http://guide/xmltv.xml".to_owned(), xmltv);
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

        let started = std::time::Instant::now();
        mgr.refresh_guide().await.expect("refresh");
        eprintln!(
            "bulk guide sync (150 ch / 5000 prog): {:?}",
            started.elapsed()
        );

        let channels = mgr
            .get_channels(&DtoOptions::default())
            .await
            .expect("chans");
        assert_eq!(channels.total_record_count, 150);
        let programs = mgr
            .get_programs(&InternalItemsQuery::default(), &DtoOptions::default())
            .await
            .expect("progs");
        assert_eq!(programs.total_record_count, 5000);
    }

    #[tokio::test]
    async fn timer_crud_roundtrips() {
        use hermit_model::live_tv::{BaseTimerInfoDto, TimerInfoDto};
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let ch = uuid::Uuid::new_v4();
        let timer = TimerInfoDto {
            base: BaseTimerInfoDto {
                channel_id: ch,
                name: Some("Record the news".to_owned()),
                start_date: parse_dt("2026-07-25T06:00:00Z").unwrap(),
                end_date: parse_dt("2026-07-25T07:00:00Z").unwrap(),
                ..BaseTimerInfoDto::default()
            },
            ..TimerInfoDto::default()
        };

        let id = mgr.create_timer(timer).await.expect("create");
        assert!(!id.is_empty());
        let timers = mgr.get_timers().await.expect("list");
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].base.channel_id, ch);
        assert_eq!(timers[0].base.name.as_deref(), Some("Record the news"));

        let got = mgr.get_timer(&id).await.expect("get").expect("some");
        assert_eq!(got.base.id.as_deref(), Some(id.as_str()));

        mgr.cancel_timer(&id).await.expect("cancel");
        assert!(mgr.get_timers().await.expect("list2").is_empty());
    }

    #[tokio::test]
    async fn series_timer_crud_and_cascade() {
        use hermit_model::live_tv::{BaseTimerInfoDto, SeriesTimerInfoDto, TimerInfoDto};
        let mgr = manager_with(FakeFetcher(HashMap::new())).await;
        let st = SeriesTimerInfoDto {
            base: BaseTimerInfoDto {
                channel_id: uuid::Uuid::new_v4(),
                name: Some("Every episode".to_owned()),
                ..BaseTimerInfoDto::default()
            },
            ..SeriesTimerInfoDto::default()
        };
        let st_id = mgr.create_series_timer(st).await.expect("create st");
        assert_eq!(mgr.get_series_timers().await.expect("list").len(), 1);

        // A timer that belongs to the series timer is removed when it's cancelled.
        let timer = TimerInfoDto {
            series_timer_id: Some(st_id.clone()),
            base: BaseTimerInfoDto {
                channel_id: uuid::Uuid::new_v4(),
                start_date: parse_dt("2026-07-25T06:00:00Z").unwrap(),
                end_date: parse_dt("2026-07-25T07:00:00Z").unwrap(),
                ..BaseTimerInfoDto::default()
            },
            ..TimerInfoDto::default()
        };
        mgr.create_timer(timer).await.expect("create timer");
        assert_eq!(mgr.get_timers().await.expect("t").len(), 1);

        mgr.cancel_series_timer(&st_id).await.expect("cancel st");
        assert!(mgr.get_series_timers().await.expect("l2").is_empty());
        assert!(
            mgr.get_timers().await.expect("t2").is_empty(),
            "cancelling a series timer drops its timers"
        );
    }
}
