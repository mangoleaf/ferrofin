//! Stored identity resolution shared by user-data writes and retention recovery.
//! The caller supplies a connection so recovery can resolve keys inside its
//! write transaction, after metadata and provider IDs have been persisted.

use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::db_error::db_err;
use crate::item_type_lookup::kind_from_type_name;
use crate::user_data_keys::{KeySource, retention_keys, user_data_keys, uses_provider_ids};

/// One item's identity fields, read once and reused for the item and (for a
/// `Season`/`Episode`) its series.
///
/// Owns its strings because it outlives the query; [`Self::as_source`] hands
/// the derivation the borrowed view it wants.
#[derive(Debug, Clone)]
struct KeyRow {
    item_id: Uuid,
    kind: BaseItemKind,
    tmdb: Option<String>,
    imdb: Option<String>,
    tvdb: Option<String>,
    custom: Option<String>,
    musicbrainz_album: Option<String>,
    musicbrainz_release_group: Option<String>,
    musicbrainz_artist: Option<String>,
    episode_title: Option<String>,
    is_series: bool,
    index_number: Option<i64>,
    parent_index_number: Option<i64>,
    name: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    extra_type: Option<String>,
    run_time_ticks: Option<i64>,
    series_id: Option<Uuid>,
}

impl KeyRow {
    fn as_source(&self) -> KeySource<'_> {
        KeySource {
            item_id: self.item_id,
            kind: self.kind,
            tmdb: self.tmdb.as_deref(),
            imdb: self.imdb.as_deref(),
            tvdb: self.tvdb.as_deref(),
            custom: self.custom.as_deref(),
            musicbrainz_album: self.musicbrainz_album.as_deref(),
            musicbrainz_release_group: self.musicbrainz_release_group.as_deref(),
            musicbrainz_artist: self.musicbrainz_artist.as_deref(),
            episode_title: self.episode_title.as_deref(),
            is_series: self.is_series,
            index_number: self.index_number,
            parent_index_number: self.parent_index_number,
            name: self.name.as_deref(),
            album: self.album.as_deref(),
            album_artist: self.album_artist.as_deref(),
            extra_type: self.extra_type.as_deref(),
            run_time_ticks: self.run_time_ticks,
        }
    }
}

/// The lowercase `ExtraType` name the C# key builder appends
/// (`ExtraType.ToString().ToLowerInvariant()`), for a stored discriminant.
pub(crate) fn extra_type_name(disc: i32) -> Option<&'static str> {
    Some(match disc {
        1 => "clip",
        2 => "trailer",
        3 => "behindthescenes",
        4 => "deletedscene",
        5 => "interview",
        6 => "scene",
        7 => "sample",
        8 => "themesong",
        9 => "themevideo",
        10 => "featurette",
        11 => "short",
        // 0 is `Unknown`, which the C# never reaches: `ExtraType.HasValue` is
        // false for a non-extra, so no key is built at all.
        _ => return None,
    })
}

/// Reads an item's identity through the same batch resolver used by scan recovery.
pub(crate) async fn load_keys(
    conn: &mut sqlx::SqliteConnection,
    item_id: Uuid,
) -> Result<Option<Vec<String>>, ServiceError> {
    Ok(load_keys_for_items(conn, &[guid_to_db(item_id)])
        .await?
        .pop()
        .map(|(_, keys)| keys))
}

/// Resolves one page, including series metadata, without per-item SQL calls.
pub(crate) async fn load_keys_for_items(
    conn: &mut sqlx::SqliteConnection,
    ids: &[String],
) -> Result<Vec<(String, Vec<String>)>, ServiceError> {
    Ok(load_identities(conn, ids)
        .await?
        .into_iter()
        .map(|identity| (identity.id, identity.keys))
        .collect())
}

pub(crate) struct StoredIdentity {
    pub id: String,
    pub keys: Vec<String>,
    pub retention_keys: Vec<String>,
}

pub(crate) async fn load_identities(
    conn: &mut sqlx::SqliteConnection,
    ids: &[String],
) -> Result<Vec<StoredIdentity>, ServiceError> {
    let mut rows = key_rows(conn, ids).await?;
    let mut series: Vec<String> = rows
        .values()
        .filter_map(|row| {
            matches!(row.kind, BaseItemKind::Season | BaseItemKind::Episode)
                .then_some(row.series_id)
                .flatten()
                .map(guid_to_db)
        })
        .filter(|id| !rows.contains_key(id))
        .collect();
    series.sort_unstable();
    series.dedup();
    rows.extend(key_rows(conn, &series).await?);
    Ok(ids
        .iter()
        .filter_map(|id| {
            let row = rows.get(id)?;
            let series = row.series_id.and_then(|id| rows.get(&guid_to_db(id)));
            let series_source = series.map(KeyRow::as_source);
            Some(StoredIdentity {
                id: id.clone(),
                keys: user_data_keys(&row.as_source(), series_source.as_ref()),
                retention_keys: retention_keys(&row.as_source(), series_source.as_ref()),
            })
        })
        .collect())
}

/// Only the identity fields, excluding large Data blobs and artwork metadata.
type RawKeyRow = (
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i32>,
    Option<i64>,
    Option<String>,
    bool,
);

async fn key_rows(
    conn: &mut sqlx::SqliteConnection,
    ids: &[String],
) -> Result<std::collections::HashMap<String, KeyRow>, ServiceError> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let json = serde_json::to_string(ids).map_err(|e| ServiceError::Backend(e.to_string()))?;
    let raw: Vec<RawKeyRow> = sqlx::query_as(
        r#"SELECT "Id", "Type", "IndexNumber", "ParentIndexNumber", "Name", "Album",
                  "AlbumArtists", "SeriesId", "ExtraType", "RunTimeTicks",
                  "EpisodeTitle", "IsSeries"
           FROM "BaseItems" WHERE "Id" IN (SELECT value FROM json_each(?1))"#,
    )
    .bind(&json)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_err)?;
    let provider_ids: Vec<&str> = raw
        .iter()
        .filter_map(|row| {
            let kind = kind_from_type_name(&row.1).unwrap_or(BaseItemKind::Folder);
            uses_provider_ids(kind).then_some(row.0.as_str())
        })
        .collect();
    let mut providers: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    if !provider_ids.is_empty() {
        let json = serde_json::to_string(&provider_ids)
            .map_err(|e| ServiceError::Backend(e.to_string()))?;
        let values: Vec<(String, String, String)> = sqlx::query_as(
            r#"SELECT "ItemId", "ProviderId", "ProviderValue" FROM "BaseItemProviders"
               WHERE "ItemId" IN (SELECT value FROM json_each(?1))"#,
        )
        .bind(json)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?;
        for (id, provider, value) in values {
            providers.entry(id).or_default().push((provider, value));
        }
    }
    let mut result = std::collections::HashMap::with_capacity(raw.len());
    for (
        id,
        type_name,
        index_number,
        parent_index_number,
        name,
        album,
        album_artists,
        series_id,
        extra_type,
        run_time_ticks,
        episode_title,
        is_series,
    ) in raw
    {
        let item_id = Uuid::parse_str(&id).map_err(|e| ServiceError::Backend(e.to_string()))?;
        let provider = |want: &str| {
            providers.get(&id).and_then(|values| {
                values
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(want))
                    .map(|(_, value)| value.clone())
            })
        };
        let row = KeyRow {
            item_id,
            kind: kind_from_type_name(&type_name).unwrap_or(BaseItemKind::Folder),
            tmdb: provider("Tmdb"),
            imdb: provider("Imdb"),
            tvdb: provider("Tvdb"),
            custom: provider("Custom"),
            musicbrainz_album: provider("MusicBrainzAlbum"),
            musicbrainz_release_group: provider("MusicBrainzReleaseGroup"),
            musicbrainz_artist: provider("MusicBrainzArtist"),
            episode_title,
            is_series,
            index_number,
            parent_index_number,
            name,
            album,
            album_artist: album_artists
                .as_deref()
                .and_then(|a| a.split('|').next())
                .filter(|a| !a.is_empty())
                .map(str::to_owned),
            extra_type: extra_type.and_then(extra_type_name).map(str::to_owned),
            run_time_ticks,
            series_id: series_id.as_deref().and_then(|s| Uuid::parse_str(s).ok()),
        };
        result.insert(id, row);
    }
    Ok(result)
}
