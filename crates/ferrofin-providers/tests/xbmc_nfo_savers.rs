//! Round-trip parity tests for the XbmcMetadata NFO savers.
//!
//! The C# `BaseNfoSaver` suite has thin direct coverage, so the oracle here is
//! the *parser*: for each real Kodi `.nfo` fixture we parse it, serialize the
//! parsed item back with the matching saver, re-parse the serialized document,
//! and assert the second parse equals the first on every field the saver emits.
//!
//! The saver deliberately defers the media-info (`<fileinfo>`) block — which
//! needs `IHasMediaSources`, absent from [`NfoBaseItem`] — and the image/user
//! data blocks (off by default in First-Light). Those source-only fields
//! (`width`, `height`, `video_3d_format`, `has_subtitles`), plus the parser-only
//! `<sortname>`/`airs_dayofweek`/`airs_time` reads that have no saver tag, are
//! zeroed on both sides before comparison. Everything else must survive the
//! round trip byte-equivalently at the model level.

use std::fs;

use chrono::TimeZone;
use ferrofin_providers::container_types::MetadataResult;
use ferrofin_providers::xbmc::base_parser::NoDirectoryService;
use ferrofin_providers::xbmc::config::NfoConfiguration;
use ferrofin_providers::xbmc::item::{NfoBaseItem, NfoItemKind};
use ferrofin_providers::xbmc::{
    StaticExternalIds, fetch_episode, fetch_movie, fetch_season, fetch_series, new_result,
};
use ferrofin_providers::{save_episode, save_movie, save_season, save_series};

/// Reads a fixture from `tests/data` by its C# `Test Data/<name>` path.
fn read_fixture(test_data_path: &str) -> String {
    let name = test_data_path.trim_start_matches("Test Data/");
    fs::read_to_string(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

/// Clears the fields the saver defers (media-info + parser-only reads), so a
/// parsed item can be compared against its serialize→reparse image.
fn project_round_trippable(item: &mut NfoBaseItem) {
    // Media-info (<fileinfo>) block is deferred (no media streams on the item).
    item.width = None;
    item.height = None;
    item.video_3d_format = None;
    item.has_subtitles = false;
    // <runtime> carries only whole minutes, and the parser may seed run time from
    // the deferred <fileinfo> <durationinseconds>; sub-minute precision cannot
    // survive, so runtime is compared out of band (see runtime_saves_whole_minutes).
    item.run_time_ticks = None;
    // Parser reads with no corresponding saver tag.
    item.sort_name = None;
    item.air_days = Vec::new();
    item.air_time = None;

    // The episode saver only emits the airs_* special-ordering tags when the item
    // is a special (ParentIndexNumber none/0); for a real season they are dropped,
    // so they cannot round-trip. Zero them unless this is a special.
    if item.parent_index_number.is_some_and(|s| s != 0) {
        item.airs_before_episode_number = None;
        item.airs_after_season_number = None;
        item.airs_before_season_number = None;
    }

    // Fields the saver only writes when non-empty: a parsed empty string cannot
    // round-trip through a skipped tag, so an empty value normalizes to None.
    empty_to_none(&mut item.tagline);
    empty_to_none(&mut item.original_title);
    empty_to_none(&mut item.custom_rating);
    empty_to_none(&mut item.official_rating);
    empty_to_none(&mut item.forced_sort_name);
    empty_to_none(&mut item.name);

    // The saver sorts multi-value collections on write, so a round trip returns
    // them sorted; compare set-equivalently by sorting both sides.
    item.genres.sort();
    item.studios.sort();
    item.tags.sort();
    // The parser overwrites ProductionLocations on each <country> element (last
    // wins), but the saver emits one <country> per location — so only a single
    // location can survive; keep just the last (sorted) one for comparison.
    item.production_locations.sort();
    if item.production_locations.len() > 1 {
        let last = item.production_locations.pop().unwrap();
        item.production_locations = vec![last];
    }

    // The saver always emits <dateadded>; a never-set DateCreated serializes as
    // the .NET DateTime.MinValue sentinel and re-parses to it, so treat that
    // sentinel as "no date".
    let min_date = chrono::Utc.with_ymd_and_hms(1, 1, 1, 0, 0, 0).unwrap();
    if item.date_created == Some(min_date) {
        item.date_created = None;
    }
}

/// Clears a string option that holds only whitespace.
fn empty_to_none(value: &mut Option<String>) {
    if value.as_deref().is_some_and(|s| s.trim().is_empty()) {
        *value = None;
    }
}

/// Normalizes a parsed result for equality: projects the item and drops the
/// non-item bookkeeping (remote/local images) the saver does not emit.
fn normalize(result: &mut MetadataResult<NfoBaseItem>) {
    project_round_trippable(&mut result.item);
    result.remote_images.clear();
    result.images.clear();
    // A never-populated people list (None) and a queried-empty one (Some([])) are
    // equivalent for the round trip; the reparse always resets to Some([]).
    if result.people.as_deref().is_none_or(<[_]>::is_empty) {
        result.people = None;
    }
    // People: keep, but the saver does not emit person provider ids / image urls
    // / item ids, so clear those to compare the parts it does round-trip. The
    // saver re-orders people (directors, then writers, then actors by sort order)
    // so compare order-independently by sorting on the round-trippable key.
    if let Some(people) = result.people.as_mut() {
        for person in people.iter_mut() {
            person.image_url = None;
            person.provider_ids.clear();
            person.id = uuid::Uuid::nil();
            person.item_id = uuid::Uuid::nil();
        }
        people.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| format!("{:?}", a.type_).cmp(&format!("{:?}", b.type_)))
        });
    }
}

/// The external-id superset advertised on both passes so every `<key>id` tag the
/// saver emits (`tmdbid`, `tvdbid`, `imdbid`/`imdb_id`) is recognized on reparse,
/// mirroring the fixed set the C# `IProviderManager` returns.
fn all_ids() -> StaticExternalIds {
    StaticExternalIds::new(["Tmdb", "Tvdb", "Imdb"])
}

/// Runs a parse→save→parse round trip and asserts the two parses match.
fn assert_round_trip(
    fixture: &str,
    kind: NfoItemKind,
    external_ids: &StaticExternalIds,
    fetch: impl Fn(&mut MetadataResult<NfoBaseItem>, &str, &str, &NfoConfiguration, &StaticExternalIds),
    save: impl Fn(&MetadataResult<NfoBaseItem>, &NfoConfiguration) -> String,
) {
    let config = NfoConfiguration::default();
    let xml = read_fixture(fixture);

    let mut first = new_result(kind);
    fetch(&mut first, fixture, &xml, &config, external_ids);

    let serialized = save(&first, &config);

    let mut second = new_result(kind);
    fetch(&mut second, fixture, &serialized, &config, external_ids);

    normalize(&mut first);
    normalize(&mut second);

    assert_eq!(
        first, second,
        "round trip mismatch for {fixture}\n--- serialized ---\n{serialized}"
    );
}

#[test]
fn movie_round_trip_justice_league() {
    assert_round_trip(
        "Test Data/Justice League.nfo",
        NfoItemKind::Movie,
        &all_ids(),
        |result, file, xml, config, ids| {
            fetch_movie(result, file, xml, config, ids, &NoDirectoryService).expect("fetch");
        },
        save_movie,
    );
}

#[test]
fn movie_round_trip_lilo_and_stitch() {
    // Exercises the '&' escaping in the collection/title and mixed provider ids.
    assert_round_trip(
        "Test Data/Lilo & Stitch.nfo",
        NfoItemKind::Movie,
        &all_ids(),
        |result, file, xml, config, ids| {
            fetch_movie(result, file, xml, config, ids, &NoDirectoryService).expect("fetch");
        },
        save_movie,
    );
}

#[test]
fn series_round_trip_american_gods() {
    assert_round_trip(
        "Test Data/American Gods.nfo",
        NfoItemKind::Series,
        &all_ids(),
        |result, file, xml, config, ids| {
            fetch_series(result, file, xml, config, ids, &NoDirectoryService).expect("fetch");
        },
        save_series,
    );
}

#[test]
fn season_round_trip() {
    assert_round_trip(
        "Test Data/Season 01.nfo",
        NfoItemKind::Season,
        &StaticExternalIds::default(),
        |result, file, xml, config, ids| {
            fetch_season(result, file, xml, config, ids, &NoDirectoryService).expect("fetch");
        },
        save_season,
    );
}

#[test]
fn episode_round_trip_the_bone_orchard() {
    assert_round_trip(
        "Test Data/The Bone Orchard.nfo",
        NfoItemKind::Episode,
        &all_ids(),
        |result, file, xml, config, ids| {
            fetch_episode(result, file, xml, config, ids, &NoDirectoryService).expect("fetch");
        },
        save_episode,
    );
}

/// A hand-built movie exercises every common-node branch the fixtures skip:
/// locked fields, tagline, critic rating, forced sort title, trailers, and the
/// generic custom-provider-id fallthrough.
#[test]
fn movie_round_trip_synthetic_full() {
    use chrono::Utc;
    use ferrofin_model::entities::MetadataField;

    let config = NfoConfiguration::default();
    let mut result = new_result(NfoItemKind::Movie);
    let item = &mut result.item;
    item.name = Some("The Title".to_owned());
    item.original_title = Some("Le Titre".to_owned());
    item.overview = Some("A \"grand\" plot & tale.".to_owned());
    item.tagline = Some("Tag & line".to_owned());
    item.custom_rating = Some("PG".to_owned());
    item.official_rating = Some("PG-13".to_owned());
    item.community_rating = Some(8.5);
    item.critic_rating = Some(77.0);
    item.production_year = Some(2001);
    item.premiere_date = Some(Utc.with_ymd_and_hms(2001, 3, 4, 0, 0, 0).unwrap());
    item.end_date = Some(Utc.with_ymd_and_hms(2001, 4, 5, 0, 0, 0).unwrap());
    item.date_created = Some(Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap());
    item.run_time_ticks = Some(120 * 600_000_000);
    item.forced_sort_name = Some("Title, The".to_owned());
    item.preferred_metadata_language = Some("en".to_owned());
    item.preferred_metadata_country_code = Some("us".to_owned());
    item.is_locked = true;
    item.locked_fields = vec![MetadataField::Name, MetadataField::Overview];
    item.genres = vec!["Drama".to_owned(), "Action".to_owned()];
    item.studios = vec!["Studio B".to_owned(), "Studio A".to_owned()];
    item.tags = vec!["tagz".to_owned(), "taga".to_owned()];
    item.production_locations = vec!["USA".to_owned(), "Canada".to_owned()];
    item.collection_name = Some("The Collection".to_owned());
    item.add_trailer_url("https://www.youtube.com/watch?v=abc123");
    item.set_provider_id("Imdb", "tt1234567");
    item.set_provider_id("Tmdb", "42");
    item.set_provider_id("Zap2It", "z99");
    item.set_provider_id("MyCustom", "custom-val");

    let serialized = save_movie(&result, &config);
    let mut second = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut second,
        "Test Data/synthetic.nfo",
        &serialized,
        &config,
        &StaticExternalIds::new(["Tmdb", "Imdb", "Zap2It", "MyCustom"]),
        &NoDirectoryService,
    )
    .expect("fetch");

    normalize(&mut result);
    normalize(&mut second);
    assert_eq!(
        result, second,
        "synthetic round trip mismatch\n--- serialized ---\n{serialized}"
    );
}
