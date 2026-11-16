//! The episode remote metadata providers: TheMovieDb's `TmdbEpisodeProvider`
//! (`MediaBrowser.Providers/Plugins/Tmdb/TV/TmdbEpisodeProvider.cs`), OMDb's
//! `OmdbEpisodeProvider` and the TVDB plugin's `TvdbEpisodeProvider`
//! (jellyfin-plugin-tvdb `5c4592f`).
//!
//! Each turns its client's answer for one episode into the provider's
//! `MetadataResult<Episode>` — a fresh item carrying only what the provider
//! sets, its people and its ids — which `MetadataService.ExecuteRemoteProviders`
//! folds into `temp` in the library's provider order. This is the mapping the
//! single-item refresh of a path-less episode folds
//! ([`crate::provider_manager`]); the library scan's own fold fills a row it
//! already holds field by field and maps the same answers.

use ferrofin_db::entities::base_items::{BaseItemEntity, PeopleEntity};
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::PersonKind;
use uuid::Uuid;

use crate::container_types::PersonInfo;
use crate::metadata_merge::MetadataResult;
use crate::omdb::{OmdbItem, OmdbPersonKind};
use crate::tmdb::{EpisodeDetails, EpisodeExtras, SeasonDetails};
use crate::tvdb::{TvdbEpisodeDetails, TvdbPerson};

/// Port of `EpisodeMetadataService.BeforeSaveInternal`'s range guard (v12.2,
/// PR 17805): an episode cannot end before it starts. An `IndexNumberEnd`
/// below the row's `IndexNumber`, or one with no `IndexNumber` at all (which
/// describes no range, and promoting it would invent an episode number), is
/// dropped from the row's `Data` with a warning. Returns whether it was, so
/// the caller treats the row as changed (`ItemUpdateType.MetadataImport`).
///
/// Rows that are not episodes, or carry no end, are left alone. The warning is
/// upstream's, and stays at warn because it names a bad source someone can
/// fix: a stored value warns once (the save that follows clears it), an NFO
/// that keeps supplying one warns on each refresh of that one file.
pub fn discard_invalid_episode_range(item: &mut BaseItemEntity) -> bool {
    // The stored type name's last segment (`…Entities.TV.Episode`): the core's
    // type lookup is out of reach from this crate.
    if item.type_.rsplit('.').next() != Some("Episode") {
        return false;
    }
    let Some(data) = item.data.as_deref() else {
        return false;
    };
    let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str::<serde_json::Value>(data)
    else {
        return false;
    };
    let Some(end) = map
        .get("IndexNumberEnd")
        .and_then(serde_json::Value::as_i64)
    else {
        return false;
    };
    let item_id = item.id.as_str();
    let path = item.path.as_deref().unwrap_or_default();
    match item.index_number {
        Some(start) if end < start => tracing::warn!(
            item_id,
            path,
            end,
            start,
            "discarding episode range end preceding episode number"
        ),
        None => tracing::warn!(
            item_id,
            path,
            end,
            "discarding episode range end without an episode number"
        ),
        Some(_) => return false,
    }
    map.remove("IndexNumberEnd");
    item.data = Some(serde_json::Value::Object(map).to_string());
    true
}

/// One provider's answer for an episode, and the language it claims
/// (`MetadataResult.ResultLanguage`): OMDb answers in English only, so its
/// overview stands only until an answer in the preferred language supplies one.
#[derive(Debug, Clone)]
pub struct EpisodeAnswer {
    /// The provider's result.
    pub result: MetadataResult,
    /// The language the answer claims, `None` for "as asked".
    pub language: Option<&'static str>,
}

/// A stored episode's range end (`Data.IndexNumberEnd`) as its lookup reads
/// it (`Episode.GetLookupInfo`): none when it is below the episode number or
/// there is none, as `EpisodeMetadataService.BeforeSaveInternal` drops it
/// before the lookup is read.
pub(crate) fn lookup_range_end(item: &BaseItemEntity) -> Option<i32> {
    let data: serde_json::Value = serde_json::from_str(item.data.as_deref()?).ok()?;
    let end = i32::try_from(data.get("IndexNumberEnd")?.as_i64()?).ok()?;
    item.index_number
        .is_some_and(|start| i64::from(end) >= start)
        .then_some(end)
}

/// The episode `TmdbEpisodeProvider.GetMetadata` reads from `season`'s listing
/// for the lookup's `IndexNumber` (`start`) and `IndexNumberEnd` (`end`),
/// `TmdbEpisodeProvider.cs:106-158`: without an end, the entry for `start`;
/// with one, the first entry from `start` to `end` the season lists (an
/// episode TMDB lacks is skipped), its name and overview followed by each
/// later one's, joined by ` / `. The rest (ids, air date, rating) and the
/// episode request for its credits, ids and videos are the first one's.
/// `None` when the season lists none of them, as for an end below `start`:
/// the caller drops a stored one first, as `BeforeSaveInternal` does.
#[must_use]
pub fn tmdb_episode_in_range(
    season: &SeasonDetails,
    start: i32,
    end: Option<i32>,
) -> Option<EpisodeDetails> {
    let Some(end) = end else {
        return season
            .episodes
            .iter()
            .find(|entry| entry.episode_number == start)
            .cloned();
    };
    // Each number once, the first listed, as `GetEpisodeAsync` asks per number.
    let mut found: Vec<&EpisodeDetails> = Vec::new();
    for entry in &season.episodes {
        if (start..=end).contains(&entry.episode_number)
            && !found
                .iter()
                .any(|kept| kept.episode_number == entry.episode_number)
        {
            found.push(entry);
        }
    }
    found.sort_by_key(|entry| entry.episode_number);
    let (first, rest) = found.split_first()?;
    let mut episode = (*first).clone();
    if !rest.is_empty() {
        // A `StringBuilder` appends a null name or overview as nothing.
        let joined = |field: fn(&EpisodeDetails) -> Option<&str>| {
            found
                .iter()
                .map(|entry| field(entry).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(" / ")
        };
        episode.name = Some(joined(|entry| entry.name.as_deref()));
        episode.overview = Some(joined(|entry| entry.overview.as_deref()));
    }
    Some(episode)
}

/// `TmdbEpisodeProvider.GetMetadata`'s answer for episode `episode_number` of
/// season `season_number`, from the season response's entry for it and the
/// episode's own request (`credits,images,external_ids,videos`).
///
/// The numbers it was looked up by, the overview, the rating (when above
/// zero), the air date and its year, the title; the episode's own `Tmdb` id
/// and the ids its external ids carry; its remote trailers; and its credits —
/// a null list when TMDB credits nobody or the request missed.
#[must_use]
pub fn tmdb_answer(
    episode: &EpisodeDetails,
    extras: Option<&EpisodeExtras>,
    season_number: i32,
    episode_number: i32,
) -> EpisodeAnswer {
    let aired = episode
        .air_date
        .as_deref()
        .and_then(crate::provider_manager::parse_ymd);
    let mut item = BaseItemEntity {
        index_number: Some(i64::from(episode_number)),
        parent_index_number: Some(i64::from(season_number)),
        name: episode.name.clone().filter(|name| !name.is_empty()),
        overview: episode.overview.clone().filter(|text| !text.is_empty()),
        community_rating: episode
            .vote_average
            .filter(|rating| *rating > 0.0)
            .map(f64::from),
        premiere_date: aired,
        production_year: aired.map(|date| i64::from(chrono::Datelike::year(&date))),
        ..BaseItemEntity::default()
    };
    let mut provider_ids = Vec::new();
    if let Some(id) = episode.tmdb_id {
        provider_ids.push(("Tmdb".to_owned(), id.to_string()));
    }
    let mut people = None;
    if let Some(extras) = extras {
        provider_ids.extend(extras.provider_ids.iter().cloned());
        let trailers: Vec<_> = extras
            .trailers
            .iter()
            .map(|trailer| (trailer.name.clone(), trailer.url.clone()))
            .collect();
        if let Some(data) = data_with_trailers(&item, &trailers) {
            item.data = Some(data);
        }
        people = extras
            .people
            .as_ref()
            .map(|people| crate::tmdb::people_entities(people))
            .filter(|people| !people.is_empty());
    }
    EpisodeAnswer {
        result: MetadataResult {
            item,
            people,
            provider_ids,
            locked_fields: Vec::new(),
        },
        language: None,
    }
}

/// The serialized `Data` of `item` with `trailers` as its `RemoteTrailers`.
fn data_with_trailers(
    item: &BaseItemEntity,
    trailers: &[(Option<String>, String)],
) -> Option<String> {
    if trailers.is_empty() {
        return None;
    }
    let mut list: Vec<serde_json::Value> = Vec::new();
    for (name, url) in trailers {
        // `AddTrailerUrl` skips a URL already added; upstream ignores case,
        // but YouTube keys are case-sensitive.
        if list.iter().any(|entry| entry["Url"] == *url) {
            continue;
        }
        // A nameless trailer has no `Name`, as Jellyfin writes no null.
        list.push(match name {
            Some(name) => serde_json::json!({ "Url": url, "Name": name }),
            None => serde_json::json!({ "Url": url }),
        });
    }
    let mut data: serde_json::Map<String, serde_json::Value> = item
        .data
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_default();
    data.insert("RemoteTrailers".to_owned(), serde_json::Value::Array(list));
    Some(serde_json::Value::Object(data).to_string())
}

/// What `OmdbItemProvider`/`OmdbEpisodeProvider` map from an OMDb record.
#[derive(Debug, Clone, Copy)]
pub struct OmdbGates {
    /// The item's metadata language is English: OMDb has no localization, so
    /// its title, rating certificate and genres are only taken then.
    pub english: bool,
    /// The metadata country is the US: the certificate is only taken then.
    pub us: bool,
    /// The OMDb plugin's `CastAndCrew` setting, which has no initializer and
    /// so defaults to off.
    pub cast_and_crew: bool,
}

/// `OmdbEpisodeProvider.GetMetadata`'s answer from the season listing's entry
/// for the episode: the title (English libraries), original language, home
/// page, plot, ratings, year, the certificate and genres (English libraries; the
/// certificate US only), the numbers it was looked up by and its `Imdb` id;
/// its people only with the plugin's `CastAndCrew`. English, as OMDb always is.
#[must_use]
pub fn omdb_answer(
    omdb: &OmdbItem,
    gates: OmdbGates,
    season_number: i32,
    episode_number: i32,
) -> EpisodeAnswer {
    let mut item = BaseItemEntity {
        index_number: Some(i64::from(episode_number)),
        parent_index_number: Some(i64::from(season_number)),
        name: omdb
            .title
            .clone()
            .filter(|title| gates.english && !title.trim().is_empty()),
        original_language: omdb.original_language(),
        overview: omdb.plot.clone(),
        community_rating: omdb.community_rating().map(f64::from),
        critic_rating: omdb.rotten_tomatoes().map(f64::from),
        production_year: omdb.production_year().map(i64::from),
        ..BaseItemEntity::default()
    };
    if let Some(website) = omdb.website.as_deref().filter(|site| !site.is_empty()) {
        let mut data = serde_json::Map::new();
        data.insert("HomePageUrl".to_owned(), serde_json::json!(website));
        item.data = Some(serde_json::Value::Object(data).to_string());
    }
    if gates.english {
        if gates.us {
            item.official_rating.clone_from(&omdb.rated);
        }
        let genres = omdb.genres();
        if !genres.is_empty() {
            item.genres = Some(genres.join("|"));
        }
    }
    let provider_ids = omdb
        .imdb_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| vec![("Imdb".to_owned(), id.to_owned())])
        .unwrap_or_default();
    let people = gates
        .cast_and_crew
        .then(|| omdb_people(omdb))
        .filter(|people| !people.is_empty());
    EpisodeAnswer {
        result: MetadataResult {
            item,
            people,
            provider_ids,
            locked_fields: Vec::new(),
        },
        language: Some("en"),
    }
}

/// Maps OMDb's credited people to persistable rows: each director, each writer,
/// then each actor. OMDb carries no per-person id or image.
#[must_use]
pub fn omdb_people(item: &OmdbItem) -> Vec<PeopleEntity> {
    item.people()
        .into_iter()
        .map(|(name, kind)| PeopleEntity {
            id: guid_to_db(Uuid::new_v4()),
            name,
            person_type: Some(
                match kind {
                    OmdbPersonKind::Director => "Director",
                    OmdbPersonKind::Writer => "Writer",
                    OmdbPersonKind::Actor => "Actor",
                }
                .to_owned(),
            ),
            role: None,
            primary_image_url: None,
            provider_id: None,
            sort_order: None,
        })
        .collect()
}

/// Maps TheTVDB's credited people to persistable rows, each added through
/// `MetadataResult.AddPerson` as the plugin's loops add them
/// ([`crate::container_types::add_person`]): names trimmed, an actor credited
/// again as a guest star in the same role merged into one guest-star credit,
/// a role-less credit given the role a later one names. The plugin sets no
/// `SortOrder` on its credits.
#[must_use]
pub fn tvdb_people(people: &[TvdbPerson]) -> Vec<PeopleEntity> {
    let mut credits = Vec::new();
    for person in people {
        crate::container_types::add_person(
            &mut credits,
            PersonInfo {
                name: person.name.clone(),
                role: person.role.clone(),
                type_: PersonKind::from_stored(&person.person_type),
                image_url: person.image_url.clone(),
                ..PersonInfo::default()
            },
        );
    }
    credits
        .into_iter()
        .map(|person| PeopleEntity {
            id: guid_to_db(person.id),
            name: person.name,
            person_type: Some(person.type_.json_name().into_owned()),
            role: person.role,
            primary_image_url: person.image_url,
            provider_id: None,
            sort_order: None,
        })
        .collect()
}

/// `TvdbEpisodeProvider.GetMetadata`'s answer for episode `episode_number` of
/// season `season_number` from TheTVDB's record for it: the numbers, the
/// original title and the title/overview in the item's metadata language
/// (already applied by [`TvdbEpisodeDetails::localize`]), the air date and its
/// year, the specials' placement (`Airs*`, for the plugin's aired order only —
/// a series with a display order of its own has none), the episode's own
/// `Tvdb` id and the `Imdb` id its remote ids carry, and the episode's own
/// credits — a list even when it credits nobody.
#[must_use]
pub fn tvdb_answer(
    episode: &TvdbEpisodeDetails,
    aired_order: bool,
    season_number: i32,
    episode_number: i32,
) -> EpisodeAnswer {
    let aired = episode
        .aired
        .as_deref()
        .and_then(crate::provider_manager::parse_ymd);
    let mut item = BaseItemEntity {
        index_number: Some(i64::from(episode_number)),
        parent_index_number: Some(i64::from(season_number)),
        name: episode.name.clone().filter(|name| !name.trim().is_empty()),
        original_title: episode.name.clone(),
        overview: episode.overview.clone(),
        premiere_date: aired,
        production_year: episode.production_year.map(i64::from),
        ..BaseItemEntity::default()
    };
    if aired_order {
        let mut data = serde_json::Map::new();
        for (key, value) in [
            ("AirsBeforeEpisodeNumber", episode.airs_before_episode),
            ("AirsAfterSeasonNumber", episode.airs_after_season),
            ("AirsBeforeSeasonNumber", episode.airs_before_season),
        ] {
            if let Some(value) = value {
                data.insert(key.to_owned(), serde_json::json!(value));
            }
        }
        if !data.is_empty() {
            item.data = Some(serde_json::Value::Object(data).to_string());
        }
    }
    let mut provider_ids = vec![("Tvdb".to_owned(), episode.tvdb_id.to_string())];
    if let Some(id) = episode.imdb_id.as_deref().filter(|id| !id.is_empty()) {
        provider_ids.push(("Imdb".to_owned(), id.to_owned()));
    }
    EpisodeAnswer {
        result: MetadataResult {
            item,
            people: Some(tvdb_people(&episode.people)),
            provider_ids,
            locked_fields: Vec::new(),
        },
        language: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmdb::TmdbPerson;

    /// A stored range end is the lookup's unless `BeforeSaveInternal` drops
    /// it: below the episode number, or with none.
    #[rstest::rstest]
    #[case::kept(Some(1), 2, Some(2))]
    #[case::one_episode(Some(2), 2, Some(2))]
    #[case::below_the_start(Some(3), 2, None)]
    #[case::without_a_start(None, 2, None)]
    fn a_stored_range_end_is_the_lookups_unless_dropped(
        #[case] start: Option<i64>,
        #[case] end: i64,
        #[case] expected: Option<i32>,
    ) {
        assert_eq!(lookup_range_end(&ranged_episode(start, end)), expected);
    }

    /// A season listing episodes 1, 2 (no overview) and 4, out of order.
    fn listed_season() -> SeasonDetails {
        let entry = |n: i32, name: &str, overview: Option<&str>| EpisodeDetails {
            tmdb_id: Some(i64::from(9000 + n)),
            episode_number: n,
            name: Some(name.to_owned()),
            overview: overview.map(str::to_owned),
            air_date: Some(format!("2011-04-0{n}")),
            ..EpisodeDetails::default()
        };
        SeasonDetails {
            episodes: vec![
                entry(2, "Two", None),
                entry(1, "One", Some("First.")),
                entry(4, "Four", Some("Fourth.")),
            ],
            ..SeasonDetails::default()
        }
    }

    /// `TmdbEpisodeProvider.GetMetadata` (v12.4 `:106-158`): a range takes
    /// every episode TMDB lists in it, the first one's id and date, and joins
    /// their names and overviews with ` / ` (a missing overview adds nothing
    /// between the separators); a range TMDB lists one episode of, or none,
    /// is that one alone, or nothing; no range is the start alone.
    #[rstest::rstest]
    #[case::single(1, None, Some(("One", Some("First."), 9001)))]
    #[case::range(1, Some(2), Some(("One / Two", Some("First. / "), 9001)))]
    #[case::gap_skipped(1, Some(4), Some(("One / Two / Four", Some("First. /  / Fourth."), 9001)))]
    #[case::first_missing(3, Some(4), Some(("Four", Some("Fourth."), 9004)))]
    #[case::none_listed(5, Some(6), None)]
    #[case::reversed(2, Some(1), None)]
    fn a_tmdb_episode_range_joins_what_the_season_lists(
        #[case] start: i32,
        #[case] end: Option<i32>,
        #[case] expected: Option<(&str, Option<&str>, i64)>,
    ) {
        let found = tmdb_episode_in_range(&listed_season(), start, end);
        assert_eq!(
            found.as_ref().map(|e| (
                e.name.as_deref().unwrap_or_default(),
                e.overview.as_deref(),
                e.tmdb_id.unwrap_or_default()
            )),
            expected
        );
    }

    /// An episode row with `IndexNumber` `start` and `Data.IndexNumberEnd` `end`.
    fn ranged_episode(start: Option<i64>, end: i64) -> BaseItemEntity {
        BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.TV.Episode".to_owned(),
            index_number: start,
            path: Some("/tv/Show/Season 1/Show - S01E01.mkv".to_owned()),
            data: Some(
                serde_json::json!({ "IndexNumberEnd": end, "AirsBeforeSeasonNumber": 2 })
                    .to_string(),
            ),
            ..BaseItemEntity::default()
        }
    }

    fn range_end(item: &BaseItemEntity) -> Option<i64> {
        serde_json::from_str::<serde_json::Value>(item.data.as_deref()?)
            .ok()?
            .get("IndexNumberEnd")?
            .as_i64()
    }

    /// v12.2 `EpisodeMetadataServiceTests`: `BeforeSave_ReversedEpisodeRange_ClearsIndexNumberEnd`
    /// (2,1 / 22,21 / 21,2 — e.g. "S03E21 - E2", where "E2" is the title),
    /// `BeforeSave_EpisodeRangeWithoutStart_ClearsIndexNumberEnd` and
    /// `BeforeSave_ValidEpisodeRange_KeepsIndexNumberEnd` (1,2 / 1,1).
    #[rstest::rstest]
    #[case::reversed(Some(2), 1, true)]
    #[case::reversed_double_digit(Some(22), 21, true)]
    #[case::title_read_as_end(Some(21), 2, true)]
    #[case::without_start(None, 2, true)]
    #[case::valid(Some(1), 2, false)]
    #[case::degenerate(Some(1), 1, false)]
    fn an_episode_range_that_ends_before_it_starts_is_discarded(
        #[case] start: Option<i64>,
        #[case] end: i64,
        #[case] discarded: bool,
    ) {
        let mut item = ranged_episode(start, end);

        assert_eq!(super::discard_invalid_episode_range(&mut item), discarded);

        assert_eq!(range_end(&item), (!discarded).then_some(end));
        assert_eq!(item.index_number, start);
        // The rest of `Data` stays.
        assert!(
            item.data
                .as_deref()
                .is_some_and(|d| d.contains("AirsBeforeSeasonNumber"))
        );
    }

    /// Only episodes carry the guard.
    #[test]
    fn a_non_episode_keeps_its_data() {
        let mut item = BaseItemEntity {
            type_: "MediaBrowser.Controller.Entities.TV.Season".to_owned(),
            ..ranged_episode(Some(2), 1)
        };
        assert!(!super::discard_invalid_episode_range(&mut item));
        assert_eq!(range_end(&item), Some(1));
    }

    fn omdb_record() -> OmdbItem {
        serde_json::from_str(
            r#"{"Title": "OMDb Episode", "Rated": "TV-MA", "Genre": "Drama, Mystery",
                "Director": "Dee", "Writer": "Wy", "Actors": "A One, A Two",
                "Plot": "Plot.", "Language": "English, French", "imdbRating": "7.7",
                "imdbID": "tt0002002", "Website": "https://home", "Year": "2011",
                "Ratings": [{"Source": "Rotten Tomatoes", "Value": "91%"}],
                "Episode": "2", "Response": "True"}"#,
        )
        .expect("an OMDb record")
    }

    #[test]
    fn a_tmdb_episode_maps_what_the_provider_sets_and_nothing_else() {
        let episode = EpisodeDetails {
            tmdb_id: Some(9002),
            episode_number: 2,
            name: Some("Title".to_owned()),
            overview: Some(String::new()),
            air_date: Some("2011-04-24".to_owned()),
            vote_average: Some(0.0),
            ..EpisodeDetails::default()
        };
        let answer = tmdb_answer(&episode, None, 1, 2);
        let item = answer.result.item;
        assert_eq!(
            (item.index_number, item.parent_index_number),
            (Some(2), Some(1))
        );
        assert_eq!(item.name.as_deref(), Some("Title"));
        // An empty synopsis and a zero rating are not the provider's to set.
        assert_eq!((item.overview, item.community_rating), (None, None));
        assert_eq!(item.production_year, Some(2011));
        assert_eq!(
            answer.result.provider_ids,
            [("Tmdb".to_owned(), "9002".to_owned())]
        );
        assert!(answer.result.people.is_none() && answer.language.is_none());

        let extras = EpisodeExtras {
            provider_ids: vec![("Imdb".to_owned(), "tt1".to_owned())],
            trailers: vec![
                crate::tmdb::TmdbTrailer {
                    name: Some("Clip".to_owned()),
                    url: "https://youtu.be/x".to_owned(),
                },
                crate::tmdb::TmdbTrailer {
                    name: None,
                    url: "https://youtu.be/y".to_owned(),
                },
                crate::tmdb::TmdbTrailer {
                    name: None,
                    url: "https://youtu.be/x".to_owned(),
                },
            ],
            people: Some(vec![TmdbPerson {
                tmdb_id: 31,
                name: "Guest".to_owned(),
                person_type: "GuestStar".to_owned(),
                role: Some("Visitor".to_owned()),
                sort_order: Some(0),
                profile_url: None,
            }]),
        };
        let answer = tmdb_answer(&episode, Some(&extras), 1, 2);
        assert!(
            answer
                .result
                .provider_ids
                .contains(&("Imdb".to_owned(), "tt1".to_owned()))
        );
        assert_eq!(answer.result.people.as_ref().map(Vec::len), Some(1));
        // Each URL once, and a nameless trailer has no `Name` key.
        let data: serde_json::Value =
            serde_json::from_str(&answer.result.item.data.expect("trailers")).unwrap();
        assert_eq!(
            data["RemoteTrailers"],
            serde_json::json!([
                {"Url":"https://youtu.be/x","Name":"Clip"},
                {"Url":"https://youtu.be/y"}
            ])
        );
        // A credits payload that was unavailable is a null list, not an empty one.
        let missing = EpisodeExtras {
            people: None,
            ..extras
        };
        assert!(
            tmdb_answer(&episode, Some(&missing), 1, 2)
                .result
                .people
                .is_none()
        );
    }

    #[test]
    fn omdb_takes_english_only_fields_for_an_english_library_and_the_certificate_for_the_us() {
        let record = omdb_record();
        let gates = |english, us, cast_and_crew| OmdbGates {
            english,
            us,
            cast_and_crew,
        };
        let en_us = omdb_answer(&record, gates(true, true, true), 1, 2);
        let item = &en_us.result.item;
        assert_eq!(item.name.as_deref(), Some("OMDb Episode"));
        assert_eq!(item.official_rating.as_deref(), Some("TV-MA"));
        assert_eq!(item.genres.as_deref(), Some("Drama|Mystery"));
        assert_eq!(item.original_language.as_deref(), Some("English"));
        assert_eq!(item.overview.as_deref(), Some("Plot."));
        assert_eq!(item.critic_rating, Some(91.0));
        assert!(
            item.data
                .as_deref()
                .is_some_and(|d| d.contains("HomePageUrl"))
        );
        assert_eq!(en_us.language, Some("en"));
        assert_eq!(
            en_us.result.provider_ids,
            [("Imdb".to_owned(), "tt0002002".to_owned())]
        );
        let names: Vec<_> = en_us
            .result
            .people
            .iter()
            .flatten()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["Dee", "Wy", "A One", "A Two"]);

        // English elsewhere: no certificate. Not English: no title, certificate
        // or genres. Without `CastAndCrew`: no people.
        let en_gb = omdb_answer(&record, gates(true, false, false), 1, 2);
        assert_eq!(en_gb.result.item.official_rating, None);
        assert!(en_gb.result.item.genres.is_some() && en_gb.result.people.is_none());
        let fr = omdb_answer(&record, gates(false, false, false), 1, 2);
        assert_eq!(
            (
                fr.result.item.name,
                fr.result.item.official_rating,
                fr.result.item.genres
            ),
            (None, None, None)
        );
        assert_eq!(fr.result.item.overview.as_deref(), Some("Plot."));
    }

    #[test]
    fn a_tvdb_episode_carries_its_own_ids_credits_and_specials_placement_for_aired_order() {
        let record = TvdbEpisodeDetails {
            tvdb_id: 5002,
            imdb_id: Some("tt0005002".to_owned()),
            airs_before_episode: Some(3),
            airs_after_season: Some(1),
            airs_before_season: None,
            name: Some("Name".to_owned()),
            overview: Some("Overview.".to_owned()),
            aired: Some("2011-04-23".to_owned()),
            production_year: Some(2011),
            people: vec![TvdbPerson {
                name: "Guest".to_owned(),
                person_type: "GuestStar".to_owned(),
                role: Some("Visitor".to_owned()),
                image_url: None,
            }],
            ..TvdbEpisodeDetails::default()
        };
        let aired = tvdb_answer(&record, true, 0, 4);
        let item = &aired.result.item;
        assert_eq!(item.original_title.as_deref(), Some("Name"));
        assert_eq!(item.production_year, Some(2011));
        let data = item.data.as_deref().expect("specials placement");
        assert!(data.contains("AirsBeforeEpisodeNumber") && data.contains("AirsAfterSeasonNumber"));
        assert!(!data.contains("AirsBeforeSeasonNumber"));
        assert_eq!(
            aired.result.provider_ids,
            [
                ("Tvdb".to_owned(), "5002".to_owned()),
                ("Imdb".to_owned(), "tt0005002".to_owned())
            ]
        );
        assert_eq!(aired.result.people.as_ref().map(Vec::len), Some(1));
        // A series with a display order of its own has no placement, and an
        // episode crediting nobody still answers with a (empty) list.
        let ordered = tvdb_answer(
            &TvdbEpisodeDetails {
                people: Vec::new(),
                ..record
            },
            false,
            1,
            2,
        );
        assert!(ordered.result.item.data.is_none());
        assert_eq!(ordered.result.people, Some(Vec::new()));
    }

    /// `MetadataResult.AddPerson` runs on TheTVDB's credits as on every
    /// provider's answer (`PeopleHelper.AddPerson`): an actor credited again as
    /// a guest star in the same role is one guest-star credit
    /// (`IsSameCredit`), a role-less credit takes the role a later one names,
    /// and names are trimmed.
    #[test]
    fn tvdb_people_merge_credits_as_add_person_does() {
        let person = |name: &str, person_type: &str, role: Option<&str>| TvdbPerson {
            name: name.to_owned(),
            person_type: person_type.to_owned(),
            role: role.map(str::to_owned),
            image_url: None,
        };
        let people = tvdb_people(&[
            person("Ann", "Actor", Some("Hero")),
            person("Ann", "GuestStar", Some("Hero")),
            person("Bo", "Actor", None),
            person("Bo", "Actor", Some("Villain")),
            person(" Cy ", "Unknown", None),
        ]);
        let rows: Vec<_> = people
            .iter()
            .map(|p| (p.name.as_str(), p.person_type.as_deref(), p.role.as_deref()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Ann", Some("GuestStar"), Some("Hero")),
                ("Bo", Some("Actor"), Some("Villain")),
                ("Cy", Some("Unknown"), None),
            ]
        );
    }
}
