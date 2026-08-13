//! [`FerrofinMediaStreamRepository`] — the concrete [`MediaStreamRepository`] over
//! `ferrofin-db`.
//!
//! Port of `MediaStreamRepository`. Reads and writes the `MediaStreamInfos`
//! table (one row per stream on an item). In C# the repository maps between the
//! ~55-column entity and the domain `MediaStream`, expanding/reversing virtual
//! paths via `IServerApplicationHost` and localizing labels via
//! `ILocalizationManager`; both are DTO-layer concerns handled downstream, so the
//! trait here works directly on [`MediaStreamInfoEntity`] rows and neither
//! sibling manager is taken as a field.
//!
//! `SaveMediaStreams` in C# deletes then re-inserts the item's streams inside a
//! transaction; that replace-in-one-transaction shape is preserved. The
//! stored `StreamType` is an `INTEGER` discriminant, mapped from the wire
//! [`MediaStreamType`] via [`crate::db_error::media_stream_type_disc`].

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::entities::MediaStreamType;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::{MediaStreamQuery, MediaStreamRepository};

use crate::db_error::{db_err, media_stream_type_disc};

/// The concrete media-stream repository.
#[derive(Clone)]
pub struct FerrofinMediaStreamRepository {
    db: Database,
}

impl std::fmt::Debug for FerrofinMediaStreamRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinMediaStreamRepository")
            .finish_non_exhaustive()
    }
}

impl FerrofinMediaStreamRepository {
    /// Creates a media-stream repository over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

/// The full ordered column list of `MediaStreamInfos`, shared by the insert
/// statement and its `VALUES` placeholder list so the two never drift.
const STREAM_COLUMNS: &str = r#"
    "ItemId", "StreamIndex", "AspectRatio", "AverageFrameRate", "BitDepth",
    "BitRate", "BlPresentFlag", "ChannelLayout", "Channels", "Codec", "CodecTag",
    "CodecTimeBase", "ColorPrimaries", "ColorSpace", "ColorTransfer", "Comment",
    "DvBlSignalCompatibilityId", "DvLevel", "DvProfile", "DvVersionMajor",
    "DvVersionMinor", "ElPresentFlag", "Hdr10PlusPresentFlag", "Height",
    "IsAnamorphic", "IsAvc", "IsDefault", "IsExternal", "IsForced",
    "IsHearingImpaired", "IsInterlaced", "KeyFrames", "Language",
    "Level", "NalLengthSize", "Path", "PixelFormat", "Profile", "RealFrameRate",
    "RefFrames", "Rotation", "RpuPresentFlag", "SampleRate", "StreamType",
    "TimeBase", "Title", "Width"
"#;

#[async_trait]
impl MediaStreamRepository for FerrofinMediaStreamRepository {
    async fn get_media_streams(
        &self,
        filter: &MediaStreamQuery,
    ) -> Result<Vec<MediaStreamInfoEntity>, ServiceError> {
        let mut sql = String::from(r#"SELECT * FROM "MediaStreamInfos" WHERE "ItemId" = ?"#);
        if filter.index.is_some() {
            sql.push_str(r#" AND "StreamIndex" = ?"#);
        }
        if filter.stream_type.is_some() {
            sql.push_str(r#" AND "StreamType" = ?"#);
        }
        sql.push_str(r#" ORDER BY "StreamIndex""#);

        let mut query =
            sqlx::query_as::<_, MediaStreamInfoEntity>(&sql).bind(guid_to_db(filter.item_id));
        if let Some(index) = filter.index {
            query = query.bind(i64::from(index));
        }
        if let Some(stream_type) = filter.stream_type {
            query = query.bind(i64::from(media_stream_type_disc(stream_type)));
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }

    async fn get_media_streams_batch(
        &self,
        item_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<MediaStreamInfoEntity>>, ServiceError> {
        let mut map: std::collections::HashMap<Uuid, Vec<MediaStreamInfoEntity>> =
            std::collections::HashMap::with_capacity(item_ids.len());
        if item_ids.is_empty() {
            return Ok(map);
        }
        // One query for the whole page; rows arrive in per-item StreamIndex order.
        for chunk in item_ids.chunks(500) {
            let ph = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT * FROM "MediaStreamInfos" WHERE "ItemId" IN ({ph})
                   ORDER BY "ItemId", "StreamIndex""#,
            );
            let mut query = sqlx::query_as::<_, MediaStreamInfoEntity>(&sql);
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for row in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Ok(id) = Uuid::parse_str(&row.item_id) {
                    map.entry(id).or_default().push(row);
                }
            }
        }
        Ok(map)
    }

    async fn get_item_ids_with_subtitles(
        &self,
        item_ids: &[Uuid],
    ) -> Result<Vec<Uuid>, ServiceError> {
        // Ids-only page query for the DTO builder's `HasSubtitles` — no stream
        // rows materialize, and `(ItemId, StreamIndex)` is the table's PK so
        // each id resolves off the index.
        let mut with_subs = Vec::new();
        for chunk in item_ids.chunks(500) {
            let ph = (2..=chunk.len() + 1)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT DISTINCT "ItemId" FROM "MediaStreamInfos"
                   WHERE "StreamType" = ?1 AND "ItemId" IN ({ph})"#,
            );
            let mut query = sqlx::query_scalar::<_, String>(&sql)
                .bind(i64::from(media_stream_type_disc(MediaStreamType::Subtitle)));
            for id in chunk {
                query = query.bind(guid_to_db(*id));
            }
            for row in query.fetch_all(self.db.pool()).await.map_err(db_err)? {
                if let Ok(id) = Uuid::parse_str(&row) {
                    with_subs.push(id);
                }
            }
        }
        Ok(with_subs)
    }

    async fn get_media_stream_languages(
        &self,
        stream_type: MediaStreamType,
    ) -> Result<Vec<String>, ServiceError> {
        // Distinct language codes for streams of this type across the library,
        // "und" (undetermined) standing in for a missing/empty language (C#).
        let rows = sqlx::query_scalar::<_, String>(
            r#"SELECT DISTINCT CASE WHEN "Language" IS NULL OR "Language" = ''
                 THEN 'und' ELSE "Language" END
               FROM "MediaStreamInfos" WHERE "StreamType" = ?1"#,
        )
        .bind(i64::from(media_stream_type_disc(stream_type)))
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows)
    }

    async fn save_media_streams(
        &self,
        item_id: Uuid,
        streams: &[MediaStreamInfoEntity],
    ) -> Result<(), ServiceError> {
        let insert_sql = format!(
            r#"INSERT INTO "MediaStreamInfos" ({STREAM_COLUMNS}) VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                ?, ?, ?)"#
        );

        let item_id_db = guid_to_db(item_id);
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "MediaStreamInfos" WHERE "ItemId" = ?1"#)
            .bind(&item_id_db)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for s in streams {
            sqlx::query(&insert_sql)
                .bind(&item_id_db)
                .bind(s.stream_index)
                .bind(&s.aspect_ratio)
                .bind(s.average_frame_rate)
                .bind(s.bit_depth)
                .bind(s.bit_rate)
                .bind(s.bl_present_flag)
                .bind(&s.channel_layout)
                .bind(s.channels)
                .bind(&s.codec)
                .bind(&s.codec_tag)
                .bind(&s.codec_time_base)
                .bind(&s.color_primaries)
                .bind(&s.color_space)
                .bind(&s.color_transfer)
                .bind(&s.comment)
                .bind(s.dv_bl_signal_compatibility_id)
                .bind(s.dv_level)
                .bind(s.dv_profile)
                .bind(s.dv_version_major)
                .bind(s.dv_version_minor)
                .bind(s.el_present_flag)
                .bind(s.hdr10_plus_present_flag)
                .bind(s.height)
                .bind(s.is_anamorphic)
                .bind(s.is_avc)
                .bind(s.is_default)
                .bind(s.is_external)
                .bind(s.is_forced)
                .bind(s.is_hearing_impaired)
                .bind(s.is_interlaced)
                .bind(&s.key_frames)
                .bind(&s.language)
                .bind(s.level)
                .bind(&s.nal_length_size)
                .bind(&s.path)
                .bind(&s.pixel_format)
                .bind(&s.profile)
                .bind(s.real_frame_rate)
                .bind(s.ref_frames)
                .bind(s.rotation)
                .bind(s.rpu_present_flag)
                .bind(s.sample_rate)
                .bind(s.stream_type)
                .bind(&s.time_base)
                .bind(&s.title)
                .bind(s.width)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::FerrofinMediaStreamRepository;
    use crate::test_support::{seed_item, test_db};
    use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_model::entities::MediaStreamType;
    use ferrofin_traits::persistence::{MediaStreamQuery, MediaStreamRepository};
    use uuid::Uuid;

    fn stream(index: i64, stream_type: i32, language: Option<&str>) -> MediaStreamInfoEntity {
        MediaStreamInfoEntity {
            item_id: String::new(),
            stream_index: index,
            aspect_ratio: None,
            average_frame_rate: None,
            bit_depth: None,
            bit_rate: None,
            bl_present_flag: None,
            channel_layout: None,
            channels: None,
            codec: Some("h264".to_owned()),
            codec_tag: None,
            codec_time_base: None,
            color_primaries: None,
            color_space: None,
            color_transfer: None,
            comment: None,
            dv_bl_signal_compatibility_id: None,
            dv_level: None,
            dv_profile: None,
            dv_version_major: None,
            dv_version_minor: None,
            el_present_flag: None,
            hdr10_plus_present_flag: None,
            height: None,
            is_anamorphic: None,
            is_avc: None,
            is_default: false,
            is_external: false,
            is_forced: false,
            is_hearing_impaired: None,
            is_interlaced: None,
            key_frames: None,
            language: language.map(str::to_owned),
            level: None,
            nal_length_size: None,
            path: None,
            pixel_format: None,
            profile: None,
            real_frame_rate: None,
            ref_frames: None,
            rotation: None,
            rpu_present_flag: None,
            sample_rate: None,
            stream_type,
            time_base: None,
            title: None,
            width: None,
        }
    }

    #[tokio::test]
    async fn save_filter_and_languages() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinMediaStreamRepository::new(db);

        // index 0 = video, index 1 = audio (eng), index 2 = subtitle (no lang).
        repo.save_media_streams(
            item,
            &[
                stream(0, 1, None),
                stream(1, 0, Some("eng")),
                stream(2, 2, None),
            ],
        )
        .await
        .expect("save");

        let all = repo
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: None,
                index: None,
            })
            .await
            .expect("all");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].stream_index, 0);

        let audio = repo
            .get_media_streams(&MediaStreamQuery {
                item_id: item,
                stream_type: Some(MediaStreamType::Audio),
                index: None,
            })
            .await
            .expect("audio");
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].language.as_deref(), Some("eng"));

        let sub_langs = repo
            .get_media_stream_languages(MediaStreamType::Subtitle)
            .await
            .expect("langs");
        assert_eq!(sub_langs, vec!["und".to_owned()]);
    }
}
