//! Stored identity resolution shared by user-data writes and retention recovery.
//! The caller supplies a connection so recovery can resolve keys inside its
//! write transaction, after metadata and provider IDs have been persisted.

use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use crate::db_error::db_err;
use crate::item_type_lookup::kind_from_type_name;
use crate::user_data_keys::{KeySource, user_data_keys, uses_provider_ids};

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

/// Reads the item and, for a
/// `Season`/`Episode` its series — and derives the keys.
pub(crate) async fn load_keys(
    conn: &mut sqlx::SqliteConnection,
    item_id: Uuid,
) -> Result<Option<Vec<String>>, ServiceError> {
    let Some(item) = key_row(conn, item_id).await? else {
        return Ok(None);
    };
    // Only a Season or an Episode consults its series, so only then is the
    // second query worth making.
    let series = match (item.kind, item.series_id) {
        (BaseItemKind::Season | BaseItemKind::Episode, Some(series_id)) => {
            key_row(conn, series_id).await?
        }
        _ => None,
    };
    let series_source = series.as_ref().map(KeyRow::as_source);
    Ok(Some(user_data_keys(
        &item.as_source(),
        series_source.as_ref(),
    )))
}

/// One item's identity fields plus its provider ids.
#[allow(
    clippy::type_complexity,
    reason = "one row read positionally; naming a struct for it would not \
              be read anywhere else"
)]
async fn key_row(
    conn: &mut sqlx::SqliteConnection,
    item_id: Uuid,
) -> Result<Option<KeyRow>, ServiceError> {
    let row: Option<(
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
        Option<bool>,
    )> = sqlx::query_as(
        r#"SELECT "Type", "IndexNumber", "ParentIndexNumber", "Name", "Album",
                  "AlbumArtists", "SeriesId", "ExtraType", "RunTimeTicks",
                  "EpisodeTitle", "IsSeries"
           FROM "BaseItems" WHERE "Id" = ?1 LIMIT 1"#,
    )
    .bind(guid_to_db(item_id))
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_err)?;

    let Some((
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
    )) = row
    else {
        return Ok(None);
    };

    let kind = kind_from_type_name(&type_name).unwrap_or(BaseItemKind::Folder);
    // Most kinds never look at a provider id, and this runs on the busiest
    // write path, so do not pay for the second query unless the derivation
    // will read it. An Episode is deliberately in the "no" list: it takes
    // its keys from the series and ignores its own providers entirely
    // (`EnableDefaultVideoUserDataKeys => false`), and episodes are the
    // bulk of a TV library.
    let providers: Vec<(String, String)> = if uses_provider_ids(kind) {
        sqlx::query_as(
            r#"SELECT "ProviderId", "ProviderValue" FROM "BaseItemProviders"
               WHERE "ItemId" = ?1"#,
        )
        .bind(guid_to_db(item_id))
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?
    } else {
        Vec::new()
    };
    let provider = |want: &str| {
        providers
            .iter()
            .find(|(id, _)| id.eq_ignore_ascii_case(want))
            .map(|(_, v)| v.clone())
    };

    Ok(Some(KeyRow {
        item_id,
        kind,
        tmdb: provider("Tmdb"),
        imdb: provider("Imdb"),
        tvdb: provider("Tvdb"),
        custom: provider("Custom"),
        musicbrainz_album: provider("MusicBrainzAlbum"),
        musicbrainz_release_group: provider("MusicBrainzReleaseGroup"),
        musicbrainz_artist: provider("MusicBrainzArtist"),
        episode_title,
        is_series: is_series.unwrap_or(false),
        index_number,
        parent_index_number,
        name,
        album,
        // `AlbumArtists` is a delimited list; the C# key uses the first.
        album_artist: album_artists
            .as_deref()
            .and_then(|a| a.split('|').next())
            .filter(|a| !a.is_empty())
            .map(str::to_owned),
        extra_type: extra_type
            .and_then(extra_type_name)
            .map(std::borrow::ToOwned::to_owned),
        run_time_ticks,
        series_id: series_id.as_deref().and_then(|s| Uuid::parse_str(s).ok()),
    }))
}
