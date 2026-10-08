//! Map a persisted item back onto the shared NFO serialization model.
use crate::xbmc::item::NfoBaseItem;
use ferrofin_db::entities::base_items::BaseItemEntity;

fn split(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|value| value.split('|'))
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

#[allow(clippy::cast_possible_truncation)] // Ratings are single precision in the NFO model.
pub(super) fn item(row: &BaseItemEntity) -> Option<NfoBaseItem> {
    let data: serde_json::Value = row
        .data
        .as_deref()
        .and_then(|data| serde_json::from_str(data).ok())
        .unwrap_or_default();
    let string = |key: &str| data[key].as_str().map(str::to_owned);
    let integer = |key: &str| {
        data[key]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
    };
    Some(NfoBaseItem {
        kind: super::policy::kind(row)?,
        name: row.name.clone(),
        original_title: row.original_title.clone(),
        sort_name: row.sort_name.clone(),
        forced_sort_name: row.forced_sort_name.clone(),
        overview: row.overview.clone(),
        tagline: row.tagline.clone(),
        critic_rating: row.critic_rating.map(|rating| rating as f32),
        community_rating: row.community_rating.map(|rating| rating as f32),
        official_rating: row.official_rating.clone(),
        custom_rating: row.custom_rating.clone(),
        preferred_metadata_language: row.preferred_metadata_language.clone(),
        preferred_metadata_country_code: row.preferred_metadata_country_code.clone(),
        production_year: row
            .production_year
            .and_then(|year| i32::try_from(year).ok()),
        premiere_date: row.premiere_date,
        end_date: row.end_date,
        date_created: row.date_created,
        run_time_ticks: row.run_time_ticks,
        is_locked: row.is_locked,
        genres: split(row.genres.as_deref()),
        studios: split(row.studios.as_deref()),
        tags: split(row.tags.as_deref()),
        production_locations: split(row.production_locations.as_deref()),
        remote_trailers: data["RemoteTrailers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|trailer| trailer["Url"].as_str().or_else(|| trailer.as_str()))
            .map(str::to_owned)
            .collect(),
        aspect_ratio: string("AspectRatio"),
        video_3d_format: serde_json::from_value(data["Video3DFormat"].clone()).ok(),
        width: row.width.and_then(|width| i32::try_from(width).ok()),
        height: row.height.and_then(|height| i32::try_from(height).ok()),
        has_subtitles: data["HasSubtitles"].as_bool().unwrap_or(false),
        collection_name: string("CollectionName"),
        artists: split(row.artists.as_deref()),
        album_artists: split(row.album_artists.as_deref()),
        album: row.album.clone(),
        display_order: string("DisplayOrder"),
        air_days: serde_json::from_value(data["AirDays"].clone()).unwrap_or_default(),
        air_time: string("AirTime"),
        status: serde_json::from_value(data["Status"].clone()).ok(),
        index_number: row
            .index_number
            .and_then(|number| i32::try_from(number).ok()),
        index_number_end: integer("IndexNumberEnd"),
        parent_index_number: row
            .parent_index_number
            .and_then(|number| i32::try_from(number).ok()),
        series_name: row.series_name.clone(),
        airs_before_episode_number: integer("AirsBeforeEpisodeNumber"),
        airs_after_season_number: integer("AirsAfterSeasonNumber"),
        airs_before_season_number: integer("AirsBeforeSeasonNumber"),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persisted_fields_and_kind_specific_data_reach_the_serializer() {
        let row=BaseItemEntity {type_:"MediaBrowser.Controller.Entities.TV.Episode".into(), name:Some("Episode".into()), original_title:Some("Original".into()), genres:Some("Drama|Comedy|".into()), artists:Some("One|Two".into()),is_locked:true,index_number:Some(2),parent_index_number:Some(1),data:Some(r#"{"IndexNumberEnd":3,"AirsBeforeSeasonNumber":2,"AirsBeforeEpisodeNumber":1,"RemoteTrailers":[{"Url":"https://example/trailer"}],"Video3DFormat":"HalfSideBySide"}"#.into()),..Default::default()};
        let item = item(&row).unwrap();
        assert_eq!(item.kind, crate::xbmc::item::NfoItemKind::Episode);
        assert_eq!(item.genres, ["Drama", "Comedy"]);
        assert_eq!(item.artists, ["One", "Two"]);
        assert_eq!(item.index_number, Some(2));
        assert_eq!(item.index_number_end, Some(3));
        assert_eq!(item.airs_before_season_number, Some(2));
        assert_eq!(item.airs_before_episode_number, Some(1));
        assert_eq!(item.remote_trailers, ["https://example/trailer"]);
        assert!(item.is_locked);
    }
}
