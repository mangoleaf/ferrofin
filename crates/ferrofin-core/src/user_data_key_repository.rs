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
    parent_id: Option<Uuid>,
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
            channel_name_key: crate::user_data_keys::live_tv_channel_name_key_enabled(),
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

/// Resolve each child's actual Series once for a browse page, regardless of
/// its own provider hydration. Empty/nil SeriesId follows the nearest Series
/// parent; a nonempty missing/non-Series ID does not fall back to parents.
pub(crate) async fn load_child_series_keys(
    conn: &mut sqlx::SqliteConnection,
    ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<String>>, ServiceError> {
    let rows = identity_rows(conn, ids).await?;
    Ok(ids
        .iter()
        .filter_map(|id| {
            let row = rows.get(id)?;
            let series = resolve_series(row, &rows)?;
            Some((id.clone(), user_data_keys(&series.as_source(), None)))
        })
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
    let rows = identity_rows(conn, ids).await?;
    Ok(ids
        .iter()
        .filter_map(|id| {
            let row = rows.get(id)?;
            let series = resolve_series(row, &rows);
            let series_source = series.map(KeyRow::as_source);
            Some(StoredIdentity {
                id: id.clone(),
                keys: user_data_keys(&row.as_source(), series_source.as_ref()),
                retention_keys: retention_keys(&row.as_source(), series_source.as_ref()),
            })
        })
        .collect())
}

/// Reuse the two existing SQL statement nodes for both page and write reads.
/// The identity query includes only necessary fallback parents; explicit Series
/// pointers are loaded together in a second batch if they were not on the page.
async fn identity_rows(
    conn: &mut sqlx::SqliteConnection,
    ids: &[String],
) -> Result<std::collections::HashMap<String, KeyRow>, ServiceError> {
    let mut rows = key_rows(conn, ids).await?;
    let mut series: Vec<String> = ids
        .iter()
        .filter_map(|id| rows.get(id))
        .filter(|row| matches!(row.kind, BaseItemKind::Season | BaseItemKind::Episode))
        .filter_map(|row| row.series_id.filter(|id| !id.is_nil()).map(guid_to_db))
        .filter(|id| !rows.contains_key(id))
        .collect();
    series.sort_unstable();
    series.dedup();
    rows.extend(key_rows(conn, &series).await?);
    Ok(rows)
}

fn resolve_series<'a>(
    row: &KeyRow,
    rows: &'a std::collections::HashMap<String, KeyRow>,
) -> Option<&'a KeyRow> {
    if !matches!(row.kind, BaseItemKind::Season | BaseItemKind::Episode) {
        return None;
    }
    if let Some(series) = row.series_id.filter(|id| !id.is_nil()) {
        return rows
            .get(&guid_to_db(series))
            .filter(|parent| parent.kind == BaseItemKind::Series);
    }
    let mut next = row.parent_id.filter(|id| !id.is_nil());
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = next {
        if !seen.insert(id) {
            break;
        }
        let parent = rows.get(&guid_to_db(id))?;
        if parent.kind == BaseItemKind::Series {
            return Some(parent);
        }
        next = parent.parent_id.filter(|id| !id.is_nil());
    }
    None
}

/// [`key_rows`]' read: the items, the series/season chain an episode or season walks for its key,
/// and their provider ids, joined through `BaseItemProviders`' primary key (one row per provider).
pub(crate) const KEY_ROWS_SQL: &str = r#"WITH RECURSIVE identity_ids("Id", "WalkParents") AS (
               SELECT "Id", CASE WHEN "Type" IN (?2, ?3)
                   AND ("SeriesId" IS NULL OR "SeriesId" = ?4) THEN 1 ELSE 0 END
               FROM "BaseItems" WHERE "Id" IN (SELECT value FROM json_each(?1))
               UNION
               SELECT parent."Id", 1 FROM identity_ids AS current
               JOIN "BaseItems" AS child ON child."Id" = current."Id"
               JOIN "BaseItems" AS parent ON parent."Id" = upper(child."ParentId")
               WHERE current."WalkParents" = 1 AND child."Type" <> ?5
           )
           SELECT b."Id", b."Type", b."IndexNumber", b."ParentIndexNumber", b."Name", b."Album",
                  b."AlbumArtists", b."SeriesId", b."ExtraType", b."RunTimeTicks",
                  b."EpisodeTitle", b."IsSeries", b."ParentId", p."ProviderId", p."ProviderValue"
           FROM "BaseItems" AS b
           LEFT JOIN "BaseItemProviders" AS p ON p."ItemId" = b."Id"
           WHERE b."Id" IN (SELECT "Id" FROM identity_ids)"#;

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
    Option<String>,
    Option<String>,
    Option<String>,
);

#[allow(
    clippy::too_many_lines,
    reason = "one repository boundary loads compact identity rows and providers together"
)]
async fn key_rows(
    conn: &mut sqlx::SqliteConnection,
    ids: &[String],
) -> Result<std::collections::HashMap<String, KeyRow>, ServiceError> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let json = serde_json::to_string(ids).map_err(|e| ServiceError::Backend(e.to_string()))?;
    let raw: Vec<RawKeyRow> = sqlx::query_as(KEY_ROWS_SQL)
        .bind(&json)
        .bind(BaseItemKind::Episode.stored_type_name())
        .bind(BaseItemKind::Season.stored_type_name())
        .bind(guid_to_db(Uuid::nil()))
        .bind(BaseItemKind::Series.stored_type_name())
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?;
    // One row per (item, provider), with the providers joined in the same read: only kinds keyed
    // by their provider ids keep them.
    let mut providers: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let mut items: std::collections::HashMap<String, RawKeyRow> =
        std::collections::HashMap::with_capacity(raw.len());
    for mut row in raw {
        let kind = kind_from_type_name(&row.1).unwrap_or(BaseItemKind::Folder);
        if let (Some(provider), Some(value)) = (row.13.take(), row.14.take())
            && uses_provider_ids(kind)
        {
            providers
                .entry(row.0.clone())
                .or_default()
                .push((provider, value));
        }
        items.entry(row.0.clone()).or_insert(row);
    }
    let mut result = std::collections::HashMap::with_capacity(items.len());
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
        parent_id,
        _,
        _,
    ) in items.into_values()
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
            parent_id: parent_id.as_deref().and_then(|s| Uuid::parse_str(s).ok()),
        };
        result.insert(id, row);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;

    /// The items, their walk and their providers come in one read, the providers through
    /// `BaseItemProviders`' key, never a scan (`EXPLAIN QUERY PLAN`).
    #[tokio::test]
    async fn the_key_read_joins_the_providers_by_their_key() {
        let db = test_db().await;
        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {KEY_ROWS_SQL}"
        )))
        .bind(r#"["x"]"#)
        .bind("a")
        .bind("b")
        .bind("c")
        .bind("d")
        .fetch_all(db.pool())
        .await
        .unwrap();
        let steps: Vec<&str> = plan.iter().map(|row| row.3.as_str()).collect();
        assert!(
            steps
                .iter()
                .any(|s| s.starts_with("SEARCH p USING") && s.contains("(ItemId=?)")),
            "{steps:?}"
        );
        assert!(
            !steps
                .iter()
                .any(|s| s.starts_with("SCAN p") || s.starts_with("SCAN b")),
            "{steps:?}"
        );
    }
}
