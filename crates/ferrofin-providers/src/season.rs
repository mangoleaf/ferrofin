//! The season remote metadata providers: TheMovieDb's `TmdbSeasonProvider`
//! (`MediaBrowser.Providers/Plugins/Tmdb/TV/TmdbSeasonProvider.cs`, master
//! `96bca6f0bd`) and the TVDB plugin's `TvdbSeasonProvider`
//! (jellyfin-plugin-tvdb `5c4592f`,
//! `Jellyfin.Plugin.Tvdb/Providers/TvdbSeasonProvider.cs`).
//!
//! Each turns its client's answer for one season into the provider's
//! `MetadataResult<Season>`: a fresh item carrying only what the provider
//! sets, its people and its ids. The library scan's provider fold and the
//! single-item refresh of a path-less season both fold these answers into
//! `temp` (`MetadataService.ExecuteRemoteProviders`, `:955-1025`), so the two
//! refresh paths map a season identically.
//!
//! Neither provider declares an `IHasOrder`, so both rank
//! [`DEFAULT_ORDER`] (`ProviderManager.GetDefaultOrder`, `:630-640`); with no
//! saved order between them, registration decides. Jellyfin registers its
//! plugins' assemblies before the server's
//! (`ApplicationHost.GetComposablePartAssemblies:881-886`), so the TVDB
//! plugin's season provider runs before TheMovieDb's, and Ferrofin registers
//! its compiled-in TheTVDB first among its built-ins for the same reason
//! ([`BUILT_IN_METADATA_FETCHERS`]) — TheTVDB first.
//!
//! [`DEFAULT_ORDER`]: crate::library_options::DEFAULT_ORDER
//! [`BUILT_IN_METADATA_FETCHERS`]: crate::library_options::BUILT_IN_METADATA_FETCHERS
//!
//! Language: upstream asks TheMovieDb in the lookup's `MetadataLanguage` and
//! claims it as the answer's `ResultLanguage` (`:43-46`); TheTVDB's season
//! answer names none, its overview being the translation in that language.
//! So neither is ever a language fallback in the fold. Ferrofin's TMDB
//! client asks in no language (TMDb's default, en-US) and names none either,
//! so in a library of another language a season's TheMovieDb overview is
//! English and still counts as the preferred one.
//! TODO(parity, open work item — NOT an accepted divergence): the TMDB
//! localisation item at the scan's `fetch_tmdb_metadata` (ask every
//! `TmdbClientManager` call in the item's language and country) covers the
//! season request too; the owner kept it out of the season port
//! (2026-10-04).

use ferrofin_db::entities::base_items::BaseItemEntity;

use crate::metadata_merge::MetadataResult;
use crate::tmdb::SeasonDetails;
use crate::tvdb::{TvdbClient, TvdbSeasonDetails};

/// `TmdbSeasonProvider.GetMetadata`'s answer (`:41-158`) for season
/// `season_number`, from the season's `/tv/{id}/season/{n}` response.
///
/// The season's number, overview, premiere date and year (its air date);
/// TMDB's season name only with the TMDb settings page's `ImportSeasonName`
/// (`:76-79`); its own `Tmdb` id and the `Tvdb` id its `external_ids` carry
/// (`:81-82`); and its credits (`:84-155`), which the provider only
/// `AddPerson`s, so an answer that credits nobody has a null `People`.
///
/// The caller decides whether there is an answer at all: none without a
/// season number or a series `Tmdb` id (no request is made), and none when
/// TMDB has no such season (`seasonResult is null`, `:62-65`).
#[must_use]
pub fn tmdb_answer(
    details: &SeasonDetails,
    season_number: i32,
    import_season_name: bool,
) -> MetadataResult {
    let aired = details
        .air_date
        .as_deref()
        .and_then(crate::provider_manager::parse_ymd);
    let item = BaseItemEntity {
        index_number: Some(i64::from(season_number)),
        overview: details.overview.clone(),
        premiere_date: aired,
        production_year: aired.map(|date| i64::from(chrono::Datelike::year(&date))),
        name: if import_season_name {
            details.name.clone()
        } else {
            None
        },
        ..BaseItemEntity::default()
    };
    let mut provider_ids = Vec::new();
    if let Some(id) = details.tmdb_id {
        provider_ids.push(("Tmdb".to_owned(), id.to_string()));
    }
    if let Some(id) = details.tvdb_id.as_deref().filter(|id| !id.is_empty()) {
        provider_ids.push(("Tvdb".to_owned(), id.to_owned()));
    }
    let people = crate::tmdb::people_entities(&details.people);
    MetadataResult {
        item,
        people: (!people.is_empty()).then_some(people),
        provider_ids,
        locked_fields: Vec::new(),
    }
}

/// `TvdbSeasonProvider.MapSeasonToResult`'s answer (`:99-123`) for season
/// `season_number`, from the season's record: the number, the overview in
/// the item's metadata `language` ([`TvdbSeasonDetails::translated_overview`],
/// matched through `three_letter_names`, the culture's ISO 639-2 codes) and
/// the season's own `Tvdb` id. The plugin never touches `People` here (a
/// null list), and names the season only with its `ImportSeasonName`
/// setting, whose default is off; Ferrofin exposes no TheTVDB settings page,
/// so the default holds and the season keeps its own name.
#[must_use]
pub fn tvdb_answer(
    record: &TvdbSeasonDetails,
    season_number: i32,
    language: &str,
    three_letter_names: &[String],
) -> MetadataResult {
    let item = BaseItemEntity {
        index_number: Some(i64::from(season_number)),
        overview: record.translated_overview(language, three_letter_names),
        ..BaseItemEntity::default()
    };
    MetadataResult {
        item,
        people: None,
        provider_ids: record
            .tvdb_id
            .map(|id| vec![("Tvdb".to_owned(), id.to_string())])
            .unwrap_or_default(),
        locked_fields: Vec::new(),
    }
}

/// `TvdbSeasonProvider.GetMetadata` (`:53-97`) for season `season_number`
/// of the series TheTVDB knows as `series_tvdb_id`, in the `season_type`
/// ordering: the season's id from the series' record
/// ([`TvdbClient::season_id`]), then the season's record, mapped by
/// [`tvdb_answer`]. `None` is no answer — TheTVDB lists no such season, or
/// a request failed (the failure is counted by the request).
///
/// The scan's `IsAutomated` is `true` (`ImageRefreshOptions`' default, which
/// a library scan and Identify keep), so the plugin always re-reads the
/// season's id from the series ("the order has changed and we need to find
/// the new season id", `:67-91`) and never resolves by an id already on the
/// season. TODO(parity, open work item — NOT an accepted divergence):
/// `ItemRefreshController` refreshes with `IsAutomated = false`
/// (`ItemRefreshController.cs:86`), where the plugin first takes the
/// season's own `Tvdb` id. To port: carry `IsAutomated` on
/// `MetadataRefreshOptions` (false from the item-refresh route) and pass the
/// season's id in here when it is false; the episode provider's
/// `IsAutomated` branch (`TvdbEpisodeProvider.cs:171`) is the same item.
///
/// The caller supplies `series_tvdb_id` only when the series has a `Tvdb`
/// id. ACCEPTED DIVERGENCE (owner, 2026-10-04; don't-port-bugs): upstream
/// also proceeds for a series that has only an IMDb or Zap2It id
/// (`IsSupported`, `:55`) and then asks for series `0`
/// (`Convert.ToInt32(null)`, `:76-80`), which TheTVDB rejects — a failed
/// request, and so an unstamped refresh, on every refresh of every such
/// season. Ferrofin makes no request there.
pub async fn tvdb_season(
    tvdb: &TvdbClient,
    series_tvdb_id: i64,
    season_type: &str,
    season_number: i32,
    language: &str,
    three_letter_names: &[String],
) -> Option<MetadataResult> {
    let season_id = tvdb
        .season_id(series_tvdb_id, season_type, season_number)
        .await?;
    let record = tvdb.season_details(season_id).await?;
    Some(tvdb_answer(
        &record,
        season_number,
        language,
        three_letter_names,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmdb::TmdbPerson;
    use crate::tvdb::TvdbTranslation;

    fn person(name: &str, order: Option<i32>) -> TmdbPerson {
        TmdbPerson {
            tmdb_id: 7,
            name: name.to_owned(),
            person_type: "Actor".to_owned(),
            role: Some("Role".to_owned()),
            sort_order: order,
            profile_url: Some("http://img/p.jpg".to_owned()),
        }
    }

    fn details() -> SeasonDetails {
        SeasonDetails {
            tmdb_id: Some(3624),
            name: Some("Season 1".to_owned()),
            overview: Some("Winter is coming.".to_owned()),
            air_date: Some("2011-04-17".to_owned()),
            tvdb_id: Some("364731".to_owned()),
            people: vec![person("Sean Bean", Some(0))],
            ..SeasonDetails::default()
        }
    }

    /// `TmdbSeasonProvider.cs:67-82`: the number it was looked up by, the
    /// overview, the air date as premiere date and year, the season's own
    /// Tmdb id and the Tvdb id of its external ids — and no name while
    /// `ImportSeasonName` is off (the default).
    #[test]
    fn tmdb_maps_the_season_and_leaves_its_name_by_default() {
        let answer = tmdb_answer(&details(), 1, false);
        assert_eq!(answer.item.index_number, Some(1));
        assert_eq!(answer.item.overview.as_deref(), Some("Winter is coming."));
        assert_eq!(
            answer.item.premiere_date.map(|d| d.to_rfc3339()),
            Some("2011-04-17T00:00:00+00:00".to_owned())
        );
        assert_eq!(answer.item.production_year, Some(2011));
        assert_eq!(answer.item.name, None, "ImportSeasonName is off");
        assert_eq!(
            answer.provider_ids,
            [
                ("Tmdb".to_owned(), "3624".to_owned()),
                ("Tvdb".to_owned(), "364731".to_owned())
            ]
        );
        let people = answer.people.expect("credits");
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].name, "Sean Bean");
        assert_eq!(people[0].provider_id, Some(7));
        assert_eq!(people[0].sort_order, Some(0));
    }

    /// `if (config.ImportSeasonName) result.Item.Name = seasonResult.Name`
    /// (`:76-79`).
    #[test]
    fn tmdb_names_the_season_with_import_season_name() {
        let answer = tmdb_answer(&details(), 1, true);
        assert_eq!(answer.item.name.as_deref(), Some("Season 1"));
    }

    /// A season with no air date, no ids and no credits: `AddPerson` never
    /// runs, so `People` stays null, and nothing else is invented.
    #[test]
    fn tmdb_answer_without_credits_has_null_people() {
        let answer = tmdb_answer(
            &SeasonDetails {
                overview: None,
                ..SeasonDetails::default()
            },
            0,
            false,
        );
        assert_eq!(answer.item.index_number, Some(0));
        assert_eq!(answer.item.premiere_date, None);
        assert_eq!(answer.item.production_year, None);
        assert!(answer.people.is_none());
        assert!(answer.provider_ids.is_empty());
    }

    fn translation(language: &str, text: Option<&str>) -> TvdbTranslation {
        TvdbTranslation {
            language: language.to_owned(),
            text: text.map(str::to_owned),
            is_alias: false,
        }
    }

    /// `MapSeasonToResult` (`:99-123`): the overview translated into the
    /// metadata language, the season's own Tvdb id, no people, no name.
    #[test]
    fn tvdb_maps_the_translated_overview_and_its_id() {
        let record = TvdbSeasonDetails {
            tvdb_id: Some(364_731),
            name: Some("Season 1".to_owned()),
            overview_translations: vec![
                translation("fra", Some("L'hiver vient.")),
                translation("eng", Some("Winter is coming.")),
            ],
            ..TvdbSeasonDetails::default()
        };
        let english = ["eng".to_owned()];
        let answer = tvdb_answer(&record, 1, "en", &english);
        assert_eq!(answer.item.index_number, Some(1));
        assert_eq!(answer.item.overview.as_deref(), Some("Winter is coming."));
        assert_eq!(answer.item.name, None);
        assert!(answer.people.is_none(), "the plugin never sets People");
        assert_eq!(
            answer.provider_ids,
            [("Tvdb".to_owned(), "364731".to_owned())]
        );
        let french = ["fre".to_owned(), "fra".to_owned()];
        assert_eq!(
            tvdb_answer(&record, 1, "fr", &french)
                .item
                .overview
                .as_deref(),
            Some("L'hiver vient.")
        );
    }

    /// No translation in the metadata language: no overview — the record has
    /// no base overview to fall back to, and the plugin's `FallbackLanguages`
    /// are empty by default. The first matching translation decides, even
    /// when it carries no text (`?.Overview`).
    #[test]
    fn tvdb_has_no_overview_without_a_matching_translation() {
        let record = TvdbSeasonDetails {
            overview_translations: vec![
                translation("deu", Some("Der Winter naht.")),
                translation("eng", None),
                translation("eng", Some("A second English entry.")),
            ],
            ..TvdbSeasonDetails::default()
        };
        assert_eq!(record.translated_overview("es", &["spa".to_owned()]), None);
        assert_eq!(record.translated_overview("en", &["eng".to_owned()]), None);
        assert!(tvdb_answer(&record, 2, "en", &[]).provider_ids.is_empty());
    }

    /// `TvdbSdkExtensions.IsMatch` (`:103-127`): TVDB's own codes for three
    /// languages, the culture's ISO 639-2 codes for the rest, nothing for a
    /// blank language.
    #[rstest::rstest]
    #[case::zh_tw("zhtw", "zh-TW", &[], true)]
    #[case::zh_tw_not_iso("zho", "zh-tw", &["zho"], false)]
    #[case::pt_br("pt", "pt-BR", &[], true)]
    #[case::pt_pt("por", "pt-PT", &["por"], true)]
    #[case::pt_pt_not_br("pt", "pt-pt", &[], false)]
    #[case::english("eng", "en", &["eng"], true)]
    #[case::english_case("ENG", "en", &["eng"], true)]
    #[case::french_bibliographic("fre", "fr", &["fre", "fra"], true)]
    #[case::french_terminological("fra", "fr", &["fre", "fra"], true)]
    #[case::other_language("deu", "en", &["eng"], false)]
    #[case::unknown_culture("eng", "xx", &[], false)]
    #[case::blank("eng", " ", &["eng"], false)]
    fn translation_languages_match_as_the_plugin_matches_them(
        #[case] translation: &str,
        #[case] language: &str,
        #[case] three_letter: &[&str],
        #[case] expected: bool,
    ) {
        let names: Vec<String> = three_letter.iter().map(|n| (*n).to_owned()).collect();
        assert_eq!(
            crate::tvdb::translation_matches(translation, language, &names),
            expected
        );
    }

    /// The whole provider over HTTP: the season's id from the series' short
    /// record (by number and ordering), then the season's record; a second
    /// season of the same series reuses the series' record; a season the
    /// series does not list asks for nothing more.
    #[tokio::test]
    async fn tvdb_season_resolves_by_the_series_record_once() {
        use std::io::{Read as _, Write as _};
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = std::sync::Arc::clone(&requests);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or_default().to_owned();
                if let Ok(mut log) = log.lock() {
                    log.push(line.clone());
                }
                let payload = if line.contains("/login") {
                    r#"{"data":{"token":"tok"}}"#
                } else if line.contains("/series/121361/extended") {
                    r#"{"data":{"seasons":[
                        {"id":11,"number":1,"type":{"type":"dvd"}},
                        {"id":12,"number":1,"type":{"type":"official"}},
                        {"id":13,"number":2,"type":{"type":"official"}}]}}"#
                } else if line.contains("/seasons/12/extended") {
                    r#"{"data":{"id":12,"translations":{"overviewTranslations":[
                        {"language":"eng","overview":"The first season."}]}}}"#
                } else if line.contains("/seasons/13/extended") {
                    r#"{"data":{"id":13}}"#
                } else {
                    "{}"
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        let tvdb = TvdbClient::new().with_base_url(&format!("http://{addr}"));
        let english = ["eng".to_owned()];
        let first = tvdb_season(&tvdb, 121_361, "official", 1, "en", &english)
            .await
            .expect("season 1");
        assert_eq!(first.item.overview.as_deref(), Some("The first season."));
        assert_eq!(first.provider_ids, [("Tvdb".to_owned(), "12".to_owned())]);
        let second = tvdb_season(&tvdb, 121_361, "OFFICIAL", 2, "en", &english)
            .await
            .expect("season 2");
        assert_eq!(second.item.overview, None);
        assert!(
            tvdb_season(&tvdb, 121_361, "official", 9, "en", &english)
                .await
                .is_none()
        );
        let asked = requests.lock().expect("log").clone();
        let series: Vec<&String> = asked
            .iter()
            .filter(|l| l.contains("/series/121361/extended"))
            .collect();
        assert_eq!(series.len(), 1, "the series record once: {asked:?}");
        assert!(series[0].contains("short=true"), "{asked:?}");
        assert!(
            asked
                .iter()
                .any(|l| l.contains("/seasons/12/extended") && l.contains("meta=translations")),
            "{asked:?}"
        );
        assert!(
            !asked.iter().any(|l| l.contains("/seasons/11/")),
            "{asked:?}"
        );
    }

    /// A series whose full record the series provider already read lists
    /// its seasons there: the season resolves with no record of its own.
    #[tokio::test]
    async fn a_season_after_its_series_reuses_the_series_record() {
        let server = crate::mock_http::MockServer::start(vec![
            ("/login", r#"{"data":{"token":"tok"}}"#.to_owned()),
            (
                "/series/5/extended",
                r#"{"data":{"name":"Show","seasons":[{"id":51,"number":1,"type":{"type":"official"}}]}}"#
                    .to_owned(),
            ),
        ])
        .await;
        let tvdb = TvdbClient::new().with_base_url(&server.base_url);
        assert!(tvdb.series_details(5, "usa").await.is_some());
        // With the stand-in gone any request fails: the id comes from the
        // record the series read.
        drop(server);
        assert_eq!(tvdb.season_id(5, "official", 1).await, Some(51));
    }
}
