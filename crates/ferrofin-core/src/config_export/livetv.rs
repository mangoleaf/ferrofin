//! Live TV as Jellyfin keeps it (gl-1556, D6): the options, tuners and guide
//! providers as `config/livetv.xml` (`LiveTvOptions`), and the timers and
//! series timers as `data/livetv/timers.json` and `seriestimers.json`
//! (`TimerManager`, `SeriesTimerManager`), from the rows the export read out
//! of Ferrofin's own tables before dropping them
//! ([`ferrofin_db::export::CarriedRows`]).
//!
//! A timer is written as upstream's `LiveTvDtoService.GetTimerInfo` maps a
//! `TimerInfoDto` to the `TimerInfo` it stores (the external ids), plus the
//! two facts the DTO does not carry: `IsManual`, and a series timer's
//! `SeriesId`, which its airings are matched by. A completed timer also takes
//! its recording's file and programme facts, which Jellyfin never refreshes
//! on a completed timer. The JSON is `JsonDefaults.Options`' shape: members as
//! declared, enums by name, nulls left out.
//!
//! Paths move onto the output's layout as the database's do
//! ([`PathRewrite::moved`]): the recording folders, `MediaLocationsCreated`,
//! a guide file and a recording's file. A tuner's `Url` never moves: every
//! channel and programme id is derived from it, so a moved one would orphan
//! every timer and favourite.

use serde_json::{Map, Value, json};

use ferrofin_db::export::{CarriedRows, PathRewrite, RecordingRow, SeriesTimerRow, TimerRow};
use ferrofin_model::live_tv::LiveTvOptions;
use ferrofin_traits::error::ServiceError;

use super::{ConfigExport, Document, ExportedFile, export_document, object, read_named};
use crate::configuration_manager::FerrofinServerConfigurationManager as Manager;

/// Where Jellyfin reads its Live TV options.
pub(super) const FILE: &str = "config/livetv.xml";

/// Where Jellyfin keeps its timers (`{DataPath}/livetv/timers.json`),
/// relative to its program data directory.
pub const TIMERS_FILE: &str = "data/livetv/timers.json";

/// Where Jellyfin keeps its series timers, relative to its program data
/// directory.
pub const SERIES_TIMERS_FILE: &str = "data/livetv/seriestimers.json";

/// The Live TV options document.
const DOCUMENT: Document = Document {
    root: "LiveTvOptions",
    written_as_default: &[],
    ferrofin_only: &[],
    entries: &[
        ("TunerHosts", "TunerHostInfo"),
        ("ListingProviders", "ListingsProviderInfo"),
        ("ChannelMappings", "NameValuePair"),
    ],
};

/// The options that hold one path: the recording folders and the program run
/// on each recording. (`MediaLocationsCreated` holds a list of them.)
const PATH_OPTIONS: [&str; 4] = [
    "RecordingPath",
    "MovieRecordingPath",
    "SeriesRecordingPath",
    "RecordingPostProcessor",
];

/// What the Live TV export wrote, for the loss report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveTvExport {
    /// The three files.
    pub files: ConfigExport,
    /// The tuners written.
    pub tuners: usize,
    /// The guide providers written.
    pub guides: usize,
    /// The timers written, completed recordings included.
    pub timers: usize,
    /// The completed recordings among them, each with its file.
    pub recordings: usize,
    /// The series timers written.
    pub series_timers: usize,
    /// Paths written as they are, outside every directory the export moves
    /// (`(setting, path)`), once each; for recordings, their folders.
    /// Jellyfin's container must reach them there.
    pub outside: Vec<(String, String)>,
    /// Tuners whose playlist is a file (`(tuner, path)`), written as it is:
    /// the channel ids derive from it, so it is not moved even when it lies
    /// in a directory the export moves.
    pub tuner_files: Vec<(String, String)>,
    /// Tuners at a loopback address (`(tuner, scheme://host:port)`, with no
    /// user, password, path or query, which may hold a credential), which
    /// inside Jellyfin's container is the container itself.
    pub tuner_loopback: Vec<(String, String)>,
    /// Rows whose stored settings could not be read, by what they are: not
    /// written.
    pub unreadable: Vec<String>,
}

/// Exports the Live TV options of the config directory `config_dir`, and the
/// tuners, guide providers, timers and series timers of `carried`, with the
/// paths they hold moved by `paths`.
///
/// # Errors
///
/// Returns [`ServiceError::Backend`] when `named/livetv.json` cannot be read
/// (one that does not parse is exported as its defaults, as the server serves
/// it), or a value cannot be written.
pub fn export_live_tv(
    config_dir: &std::path::Path,
    carried: &CarriedRows,
    paths: &PathRewrite,
) -> Result<LiveTvExport, ServiceError> {
    let named = config_dir.join("users").join(Manager::NAMED_SUBDIR);
    let options: LiveTvOptions = read_named(&named.join(Manager::LIVE_TV_FILE_NAME))?;
    let mut export = LiveTvExport::default();
    let mut fields = object(&options)?;
    for setting in PATH_OPTIONS {
        if let Some(Value::String(path)) = fields.get_mut(setting) {
            *path = move_path(paths, setting, path, &mut export.outside);
        }
    }
    if let Some(Value::Array(locations)) = fields.get_mut("MediaLocationsCreated") {
        for location in locations {
            if let Value::String(path) = location {
                *path = move_path(paths, "MediaLocationsCreated", path, &mut export.outside);
            }
        }
    }
    let tuners = rows(&carried.tuner_hosts, "a tuner", &mut export.unreadable);
    for tuner in &tuners {
        let Some(Value::String(url)) = tuner.get("Url") else {
            continue;
        };
        let name = ["FriendlyName", "Id"]
            .iter()
            .find_map(|key| {
                tuner
                    .get(*key)
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
            })
            .unwrap_or_default();
        // A URL with no scheme is a path, as the tuner reads it, relative ones too.
        if !url.is_empty() && !url.contains("://") {
            export.tuner_files.push((name.to_owned(), url.clone()));
        } else if let Some(origin) = loopback_origin(url) {
            export.tuner_loopback.push((name.to_owned(), origin));
        }
    }
    let mut guides = rows(
        &carried.listing_providers,
        "a guide provider",
        &mut export.unreadable,
    );
    for guide in &mut guides {
        if let Some(Value::String(path)) = guide.get_mut("Path") {
            *path = move_path(paths, "a guide provider's Path", path, &mut export.outside);
        }
    }
    export.tuners = tuners.len();
    export.guides = guides.len();
    fields.insert(
        "TunerHosts".to_owned(),
        Value::Array(tuners.into_iter().map(Value::Object).collect()),
    );
    fields.insert(
        "ListingProviders".to_owned(),
        Value::Array(guides.into_iter().map(Value::Object).collect()),
    );
    export_document(
        FILE,
        &DOCUMENT,
        &Value::Object(fields),
        &Value::Object(object(&LiveTvOptions::default())?),
        &mut export.files,
    )?;

    let mut timers = Vec::new();
    for row in &carried.timers {
        match timer(row, paths, &mut export.outside) {
            Some(timer) => {
                export.recordings += usize::from(timer.contains_key("RecordingPath"));
                timers.push(Value::Object(timer));
            }
            None => export.unreadable.push("a timer".to_owned()),
        }
    }
    let mut series = Vec::new();
    for row in &carried.series_timers {
        match series_timer(row) {
            Some(timer) => series.push(Value::Object(timer)),
            None => export.unreadable.push("a series timer".to_owned()),
        }
    }
    export.timers = timers.len();
    export.series_timers = series.len();
    for (path, list) in [(TIMERS_FILE, timers), (SERIES_TIMERS_FILE, series)] {
        export.files.files.push(ExportedFile {
            path: path.to_owned(),
            contents: Value::Array(list).to_string(),
            private: false,
        });
    }
    // A guide provider's password (Schedules Direct) is in it.
    for file in &mut export.files.files {
        file.private |= file.path == FILE;
    }
    Ok(export)
}

/// `scheme://host[:port]` of `url` when its host is a loopback address
/// (`localhost`, `127.x.x.x`, `[::1]`): nothing of its user, password, path or
/// query, which may carry a credential.
fn loopback_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if host_port.starts_with('[') {
        host_port
            .split_once(']')
            .map_or(host_port, |(host, _)| host)
            .trim_start_matches('[')
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    loopback.then(|| format!("{scheme}://{host_port}"))
}

/// `path` moved onto the output's layout; one outside every directory the
/// export moves stays, and an absolute one is listed under `setting`.
fn move_path(
    paths: &PathRewrite,
    setting: &str,
    path: &str,
    outside: &mut Vec<(String, String)>,
) -> String {
    if let Some(moved) = paths.moved(path) {
        return moved;
    }
    if path.starts_with('/') && !outside.iter().any(|(s, p)| s == setting && p == path) {
        outside.push((setting.to_owned(), path.to_owned()));
    }
    path.to_owned()
}

/// Each stored document of `rows` as an object without its null members,
/// which `XmlSerializer` leaves out; one that is no object is listed as
/// `what` in `unreadable`.
fn rows(rows: &[String], what: &str, unreadable: &mut Vec<String>) -> Vec<Map<String, Value>> {
    rows.iter()
        .filter_map(|data| {
            if let Some(mut fields) = dto(data) {
                fields.retain(|_, value| !value.is_null());
                Some(fields)
            } else {
                unreadable.push(what.to_owned());
                None
            }
        })
        .collect()
}

/// A stored DTO as an object, or `None` when it is not one.
fn dto(data: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str(data) {
        Ok(Value::Object(fields)) => Some(fields),
        _ => None,
    }
}

/// `TimerInfo` from the stored timer `row`: upstream's `GetTimerInfo` on its
/// DTO, its `IsManual`, and for a recording its file and programme facts.
fn timer(
    row: &TimerRow,
    paths: &PathRewrite,
    outside: &mut Vec<(String, String)>,
) -> Option<Map<String, Value>> {
    let dto = dto(&row.data)?;
    let get = |key: &str| dto.get(key).filter(|v| !v.is_null()).cloned();
    let mut timer = json!({
        "ProviderIds": {},
        "SeriesProviderIds": {},
        "Tags": [],
        "Id": get("ExternalId")
            .filter(|id| id.as_str().is_some_and(|id| !id.is_empty()))
            .unwrap_or_else(|| Value::String(row.id.clone())),
    });
    let fields = timer.as_object_mut()?;
    for (to, from) in [
        ("SeriesTimerId", "ExternalSeriesTimerId"),
        ("ChannelId", "ExternalChannelId"),
        ("ProgramId", "ExternalProgramId"),
        ("Name", "Name"),
        ("Overview", "Overview"),
        ("StartDate", "StartDate"),
        ("EndDate", "EndDate"),
        ("Status", "Status"),
    ] {
        if let Some(value) = get(from) {
            fields.insert(to.to_owned(), value);
        }
    }
    for (key, default) in [
        ("PrePaddingSeconds", json!(0)),
        ("PostPaddingSeconds", json!(0)),
        ("IsPrePaddingRequired", json!(false)),
        ("IsPostPaddingRequired", json!(false)),
    ] {
        fields.insert(key.to_owned(), get(key).unwrap_or(default));
    }
    fields.insert("IsManual".to_owned(), Value::Bool(row.is_manual));
    fields.insert(
        "Priority".to_owned(),
        get("Priority").unwrap_or_else(|| json!(0)),
    );
    fields.insert("RetryCount".to_owned(), json!(0));
    let recording = row.recording.as_ref().filter(|r| r.status == "Completed");
    programme_facts(fields, recording);
    if let Some(path) = recording.and_then(|r| r.path.as_deref()) {
        let moved = paths.moved(path).unwrap_or_else(|| {
            // One line per folder, not per recording.
            let folder = std::path::Path::new(path)
                .parent()
                .map_or(path, |p| p.to_str().unwrap_or(path));
            move_path(paths, "Recordings folder", folder, outside);
            path.to_owned()
        });
        fields.insert("RecordingPath".to_owned(), Value::String(moved));
    }
    fields.insert(
        "KeepUntil".to_owned(),
        get("KeepUntil").unwrap_or_else(|| json!("UntilDeleted")),
    );
    Some(std::mem::take(fields))
}

/// The programme members of `TimerInfo`: their defaults, or for a completed
/// recording the facts it kept (`CopyProgramInfoToTimerInfo`'s, with its
/// `Tags` in `GuideManager`'s order).
fn programme_facts(fields: &mut Map<String, Value>, recording: Option<&RecordingRow>) {
    let default = RecordingRow::default();
    let r = recording.unwrap_or(&default);
    if let Some(n) = r.season_number {
        fields.insert("SeasonNumber".to_owned(), json!(n));
    }
    if let Some(n) = r.episode_number {
        fields.insert("EpisodeNumber".to_owned(), json!(n));
    }
    fields.insert("IsMovie".to_owned(), json!(r.is_movie));
    fields.insert("IsKids".to_owned(), json!(r.is_kids));
    fields.insert("IsSports".to_owned(), json!(r.is_sports));
    fields.insert("IsNews".to_owned(), json!(r.is_news));
    fields.insert("IsSeries".to_owned(), json!(r.is_series));
    if let Some(year) = r.production_year {
        fields.insert("ProductionYear".to_owned(), json!(year));
    }
    if let Some(title) = &r.episode_title {
        fields.insert("EpisodeTitle".to_owned(), json!(title));
    }
    fields.insert("IsProgramSeries".to_owned(), json!(r.is_series));
    fields.insert("IsRepeat".to_owned(), json!(r.is_repeat));
    fields.insert("Genres".to_owned(), json!([]));
    let tags: Vec<&str> = [
        (r.is_live, "Live"),
        (r.is_premiere, "Premiere"),
        (r.is_news, "News"),
        (r.is_sports, "Sports"),
        (r.is_kids, "Kids"),
        (r.is_repeat, "Repeat"),
        (r.is_movie, "Movie"),
        (r.is_series, "Series"),
    ]
    .into_iter()
    .filter_map(|(set, tag)| set.then_some(tag))
    .collect();
    fields.insert("Tags".to_owned(), json!(tags));
}

/// `SeriesTimerInfo` from the stored series timer `row`: upstream's
/// `GetSeriesTimerInfo` on its DTO, the external id it was derived from, and
/// its `SeriesId`.
fn series_timer(row: &SeriesTimerRow) -> Option<Map<String, Value>> {
    let dto = dto(&row.data)?;
    let get = |key: &str| dto.get(key).filter(|v| !v.is_null()).cloned();
    let id = Some(row.external_id.as_str())
        .filter(|id| !id.is_empty())
        .map(|id| Value::String(id.to_owned()))
        .or_else(|| get("ExternalId"))
        .or_else(|| get("Id"))?;
    let mut fields = Map::new();
    fields.insert("Id".to_owned(), id);
    for (to, from) in [
        ("ChannelId", "ExternalChannelId"),
        ("ProgramId", "ExternalProgramId"),
        ("Name", "Name"),
        ("Overview", "Overview"),
        ("StartDate", "StartDate"),
        ("EndDate", "EndDate"),
    ] {
        if let Some(value) = get(from) {
            fields.insert(to.to_owned(), value);
        }
    }
    for (key, default) in [
        ("RecordAnyTime", json!(false)),
        ("RecordAnyChannel", json!(false)),
        ("KeepUpTo", json!(0)),
        ("KeepUntil", json!("UntilDeleted")),
        ("SkipEpisodesInLibrary", json!(true)),
        ("RecordNewOnly", json!(false)),
        ("Days", json!([])),
        ("Priority", json!(0)),
        ("PrePaddingSeconds", json!(0)),
        ("PostPaddingSeconds", json!(0)),
        ("IsPrePaddingRequired", json!(false)),
        ("IsPostPaddingRequired", json!(false)),
    ] {
        fields.insert(key.to_owned(), get(key).unwrap_or(default));
    }
    if let Some(series) = row.series_id.as_deref().filter(|id| !id.is_empty()) {
        fields.insert("SeriesId".to_owned(), json!(series));
    }
    Some(fields)
}

#[cfg(test)]
mod tests {
    use ferrofin_common::config_import::{LIVETV_OPTIONS_XML_DENY, LIVETV_XML_DENY, import_over};
    use ferrofin_db::export::MirroredLiveTv;

    use super::*;

    /// What Jellyfin 12.4 wrote for one M3U tuner and one XMLTV guide, a film
    /// timer and a series timer (`testdata/`, from `jellyfin/jellyfin:12.4`).
    const GOLDEN_XML: &str = include_str!("testdata/livetv.xml");
    const GOLDEN_TIMERS: &str = include_str!("testdata/timers.json");
    const GOLDEN_SERIES: &str = include_str!("testdata/seriestimers.json");

    /// A config directory whose `named/livetv.json` is `options`.
    fn config_dir(options: &LiveTvOptions) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let named = dir.path().join("users").join("named");
        std::fs::create_dir_all(&named).expect("named");
        std::fs::write(
            named.join("livetv.json"),
            serde_json::to_vec(options).expect("serialize"),
        )
        .expect("write");
        dir
    }

    /// The rows Ferrofin stores for the golden server's tuner and guide, as
    /// the adoption import stores them (`live_tv_import`).
    fn golden_rows() -> CarriedRows {
        let options: LiveTvOptions = import_over(
            &LiveTvOptions::default(),
            GOLDEN_XML,
            "LiveTvOptions",
            LIVETV_XML_DENY,
        )
        .expect("imports");
        CarriedRows {
            tuner_hosts: options
                .tuner_hosts
                .iter()
                .map(|t| serde_json::to_string(t).expect("serialize"))
                .collect(),
            listing_providers: options
                .listing_providers
                .iter()
                .map(|p| serde_json::to_string(p).expect("serialize"))
                .collect(),
            ..CarriedRows::default()
        }
    }

    fn file<'a>(export: &'a LiveTvExport, path: &str) -> &'a str {
        &export
            .files
            .files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("{path} is exported"))
            .contents
    }

    /// Ferrofin's options, tuner and guide, as adoption brought them in from
    /// Jellyfin's own `livetv.xml`, go back as the document Jellyfin wrote:
    /// the same members, entries and values.
    #[test]
    fn livetv_xml_has_the_shape_jellyfin_writes() {
        let scalar: LiveTvOptions = import_over(
            &LiveTvOptions::default(),
            GOLDEN_XML,
            "LiveTvOptions",
            LIVETV_OPTIONS_XML_DENY,
        )
        .expect("imports");
        let dir = config_dir(&scalar);
        let export =
            export_live_tv(dir.path(), &golden_rows(), &PathRewrite::default()).expect("exports");
        let (root, ours) = super::super::tests::members(file(&export, FILE));
        let (golden_root, mut theirs) = super::super::tests::members(GOLDEN_XML);
        assert_eq!(root, golden_root);
        // A null `int?`: `XmlSerializer` writes it nil, the export leaves it
        // out, which reads back as the same null.
        assert!(theirs.remove("GuideDays").is_some_and(|days| days.nil));
        assert!(!ours.contains_key("GuideDays"));
        let differ: Vec<_> = theirs
            .keys()
            .chain(ours.keys())
            .filter(|member| ours.get(*member) != theirs.get(*member))
            .map(|member| (member, ours.get(member), theirs.get(member)))
            .collect();
        assert!(differ.is_empty(), "members that differ: {differ:#?}");
        assert_eq!((export.tuners, export.guides), (1, 1));
        assert!(
            export
                .files
                .files
                .iter()
                .any(|f| f.path == FILE && f.private)
        );
        assert_eq!(export.files.not_exported, []);
    }

    /// A `null` element in a guide provider's lists (gl-1677), as Jellyfin
    /// stores it (`<string xsi:nil="true" />`, `<NameValuePair xsi:nil="true"
    /// />`), comes in as a null element and goes back out as the same nil
    /// element, so the file reads back as Jellyfin wrote it.
    #[test]
    fn a_nil_list_element_round_trips_through_livetv_xml() {
        let xml = GOLDEN_XML
            .replacen(
                "<EnabledTuners />",
                "<EnabledTuners><string xsi:nil=\"true\" /><string>t1</string></EnabledTuners>",
                1,
            )
            .replacen(
                "<ChannelMappings />",
                "<ChannelMappings><NameValuePair xsi:nil=\"true\" /></ChannelMappings>",
                1,
            );
        let options: LiveTvOptions = import_over(
            &LiveTvOptions::default(),
            &xml,
            "LiveTvOptions",
            LIVETV_XML_DENY,
        )
        .expect("imports");
        let provider = &options.listing_providers[0];
        assert_eq!(provider.enabled_tuners, [None, Some("t1".to_owned())]);
        assert_eq!(provider.channel_mappings, [None]);

        let rows = CarriedRows {
            listing_providers: vec![serde_json::to_string(provider).expect("serialize")],
            ..CarriedRows::default()
        };
        let dir = config_dir(&LiveTvOptions::default());
        let export = export_live_tv(dir.path(), &rows, &PathRewrite::default()).expect("exports");
        let written = file(&export, FILE);
        let back: LiveTvOptions = import_over(
            &LiveTvOptions::default(),
            written,
            "LiveTvOptions",
            LIVETV_XML_DENY,
        )
        .expect("reads back");
        assert_eq!(
            back.listing_providers[0].enabled_tuners,
            provider.enabled_tuners
        );
        assert_eq!(
            back.listing_providers[0].channel_mappings,
            provider.channel_mappings
        );
        assert!(written.contains("<string xsi:nil=\"true\" />"), "{written}");
        assert!(
            written.contains("<NameValuePair xsi:nil=\"true\" />"),
            "{written}"
        );
    }

    /// A guide provider with ONE channel mapping, as Jellyfin writes it
    /// (`<ChannelMappings><NameValuePair>…</NameValuePair></ChannelMappings>`),
    /// imports with that mapping: a one-entry list of `NameValuePair` is a list,
    /// not an object with a `NameValuePair` member.
    #[test]
    fn a_provider_with_one_channel_mapping_imports() {
        let xml = GOLDEN_XML.replacen(
            "<ChannelMappings />",
            "<ChannelMappings><NameValuePair><Name>m3u_one</Name><Value>one.tv</Value>\
             </NameValuePair></ChannelMappings>",
            1,
        );
        let options: LiveTvOptions = import_over(
            &LiveTvOptions::default(),
            &xml,
            "LiveTvOptions",
            LIVETV_XML_DENY,
        )
        .expect("imports");
        let [provider] = options.listing_providers.as_slice() else {
            panic!(
                "expected the one provider, got {:?}",
                options.listing_providers
            );
        };
        assert_eq!(
            provider.channel_mappings,
            [Some(ferrofin_model::dto::NameValuePair::new(
                "m3u_one", "one.tv"
            ))]
        );
    }

    /// The golden film timer as Ferrofin stores it: its `TimerInfoDto`, the
    /// internal ids beside the external ones.
    const FILM_DTO: &str = r#"{"Id":"0f6c8a6e3c2b4f0e9a1d2c3b4a5f6e7d","Type":"Timer","ServerId":"x",
        "ExternalId":"f5301b18ffda4b789f7335bca4545543",
        "ChannelId":"9b5c0a5f-1b5d-4a1e-8a43-0e0a3a7e2b11",
        "ExternalChannelId":"m3u_9f96fb22c5bd3f691eabd2b3ee26580e8c1bbe13af2af30f3026ca905d2008d6",
        "ChannelName":"Test Films","ProgramId":"5d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a",
        "ExternalProgramId":"films.test_2026-11-22T20:00:00.0000000+00:00_m3u_9f96fb22c5bd3f691eabd2b3ee26580e8c1bbe13af2af30f3026ca905d2008d6",
        "Name":"The Long Road","Overview":"A film.",
        "StartDate":"2026-11-22T20:00:00.0000000Z","EndDate":"2026-11-22T22:00:00.0000000Z",
        "ServiceName":"Emby","Priority":0,"PrePaddingSeconds":0,"PostPaddingSeconds":0,
        "IsPrePaddingRequired":false,"IsPostPaddingRequired":false,"KeepUntil":"UntilDeleted",
        "Status":"New","SeriesTimerId":null,"RunTimeTicks":72000000000}"#;

    /// A scheduled timer is the file entry Jellyfin wrote for it, but for what
    /// Jellyfin's guide refresh copies from the programme into it.
    #[test]
    fn a_timer_is_what_jellyfin_writes_before_its_guide_refresh() {
        let dir = config_dir(&LiveTvOptions::default());
        let carried = CarriedRows {
            timers: vec![TimerRow {
                id: "0f6c8a6e3c2b4f0e9a1d2c3b4a5f6e7d".to_owned(),
                data: FILM_DTO.to_owned(),
                is_manual: true,
                recording: None,
            }],
            ..CarriedRows::default()
        };
        let export =
            export_live_tv(dir.path(), &carried, &PathRewrite::default()).expect("exports");
        let ours: Value = serde_json::from_str(file(&export, TIMERS_FILE)).expect("json");
        let golden: Vec<Value> = serde_json::from_str(GOLDEN_TIMERS).expect("json");
        let mut film = golden
            .into_iter()
            .find(|t| t["Name"] == "The Long Road")
            .expect("the film");
        // Copied from the programme at the refresh (`CopyProgramInfoToTimerInfo`).
        film["ProviderIds"] = json!({});
        film["Tags"] = json!([]);
        film["IsMovie"] = json!(false);
        film.as_object_mut()
            .expect("object")
            .remove("ProductionYear");
        assert_eq!(ours, json!([film]));
        assert_eq!((export.timers, export.recordings), (1, 0));
    }

    /// A series timer is exactly the entry Jellyfin wrote: its id the
    /// external one it was derived from, and its `SeriesId` from its column.
    #[test]
    fn a_series_timer_is_what_jellyfin_writes() {
        let golden: Vec<Value> = serde_json::from_str(GOLDEN_SERIES).expect("json");
        let mut dto = golden[0].clone();
        let fields = dto.as_object_mut().expect("object");
        let external = fields.remove("Id").expect("id");
        let series_id = fields.remove("SeriesId").expect("series id");
        for key in ["ChannelId", "ProgramId"] {
            let value = fields.remove(key).expect("external id");
            fields.insert(format!("External{key}"), value);
        }
        fields.insert("Id".to_owned(), json!("internal"));
        fields.insert(
            "ChannelId".to_owned(),
            json!("9b5c0a5f-1b5d-4a1e-8a43-0e0a3a7e2b11"),
        );
        fields.insert("ImageTags".to_owned(), json!({}));
        let dir = config_dir(&LiveTvOptions::default());
        let carried = CarriedRows {
            series_timers: vec![SeriesTimerRow {
                data: dto.to_string(),
                external_id: external.as_str().expect("text").to_owned(),
                series_id: series_id.as_str().map(str::to_owned),
            }],
            ..CarriedRows::default()
        };
        let export =
            export_live_tv(dir.path(), &carried, &PathRewrite::default()).expect("exports");
        let ours: Value = serde_json::from_str(file(&export, SERIES_TIMERS_FILE)).expect("json");
        assert_eq!(ours, Value::Array(golden));
    }

    /// A completed recording keeps its file, moved onto the layout, and the
    /// programme facts Jellyfin never refreshes on it; its tags in the guide
    /// refresh's order.
    #[test]
    fn a_completed_recording_keeps_its_file_and_programme_facts() {
        let dir = config_dir(&LiveTvOptions::default());
        let paths = PathRewrite {
            moves: vec![("/srv/ff".to_owned(), "/config".to_owned())],
            ..PathRewrite::default()
        };
        let recording = RecordingRow {
            status: "Completed".to_owned(),
            path: Some("/srv/ff/data/livetv/recordings/Quiz Night/S01E02.ts".to_owned()),
            episode_title: Some("Round 2".to_owned()),
            season_number: Some(1),
            episode_number: Some(2),
            production_year: Some(2026),
            is_series: true,
            is_kids: true,
            is_premiere: true,
            ..RecordingRow::default()
        };
        let carried = CarriedRows {
            timers: vec![
                TimerRow {
                    id: "a".to_owned(),
                    data: FILM_DTO.replace(r#""Status":"New""#, r#""Status":"Completed""#),
                    is_manual: false,
                    recording: Some(recording.clone()),
                },
                // A capture a crash left: mapped as it stands, no file.
                TimerRow {
                    id: "b".to_owned(),
                    data: FILM_DTO.to_owned(),
                    is_manual: false,
                    recording: Some(RecordingRow {
                        status: "InProgress".to_owned(),
                        ..recording
                    }),
                },
                TimerRow {
                    id: "c".to_owned(),
                    data: "not json".to_owned(),
                    ..TimerRow::default()
                },
            ],
            mirrored: MirroredLiveTv::default(),
            ..CarriedRows::default()
        };
        let export = export_live_tv(dir.path(), &carried, &paths).expect("exports");
        let timers: Vec<Value> = serde_json::from_str(file(&export, TIMERS_FILE)).expect("json");
        let completed = &timers[0];
        assert_eq!(
            completed["RecordingPath"],
            "/config/data/livetv/recordings/Quiz Night/S01E02.ts"
        );
        assert_eq!(completed["Tags"], json!(["Premiere", "Kids", "Series"]));
        assert_eq!(completed["EpisodeTitle"], "Round 2");
        assert_eq!(
            (
                completed["SeasonNumber"].clone(),
                completed["EpisodeNumber"].clone()
            ),
            (json!(1), json!(2))
        );
        assert_eq!(completed["IsProgramSeries"], true);
        assert_eq!(completed["IsKids"], true);
        assert!(timers[1].get("RecordingPath").is_none(), "{}", timers[1]);
        assert_eq!(timers[1]["Tags"], json!([]));
        assert_eq!((export.timers, export.recordings), (2, 1));
        assert_eq!(export.unreadable, ["a timer"]);
    }

    /// The recording folders and a guide file move with the layout; a tuner's
    /// playlist never does, and is listed; a path outside every directory the
    /// export moves stays and is listed.
    #[test]
    fn paths_move_onto_the_layout_but_a_tuners_url() {
        let options = LiveTvOptions {
            recording_path: Some("/srv/ff/data/livetv/recordings".to_owned()),
            movie_recording_path: Some("/mnt/films".to_owned()),
            recording_post_processor: Some("/srv/ff/scripts/post.sh".to_owned()),
            media_locations_created: vec!["/srv/ff/data/livetv/recordings".to_owned()],
            ..LiveTvOptions::default()
        };
        let dir = config_dir(&options);
        let paths = PathRewrite {
            moves: vec![("/srv/ff".to_owned(), "/config".to_owned())],
            ..PathRewrite::default()
        };
        let carried = CarriedRows {
            tuner_hosts: vec![
                r#"{"Id":"t","Url":"/srv/ff/data/tuner.m3u","Type":"m3u","FriendlyName":"Attic","DeviceId":null}"#
                    .to_owned(),
                r#"{"Id":"l","Url":"http://127.0.0.1:5004/lineup.m3u","Type":"m3u","FriendlyName":""}"#
                    .to_owned(),
                r#"{"Id":"n","Url":"http://tuner.lan/lineup.m3u","Type":"m3u"}"#.to_owned(),
            ],
            listing_providers: vec![
                r#"{"Id":"g","Type":"xmltv","Path":"/srv/ff/data/guide.xml"}"#.to_owned(),
                "[]".to_owned(),
            ],
            ..CarriedRows::default()
        };
        let export = export_live_tv(dir.path(), &carried, &paths).expect("exports");
        let xml = file(&export, FILE);
        for expected in [
            "<RecordingPath>/config/data/livetv/recordings</RecordingPath>",
            "<string>/config/data/livetv/recordings</string>",
            "<MovieRecordingPath>/mnt/films</MovieRecordingPath>",
            "<RecordingPostProcessor>/config/scripts/post.sh</RecordingPostProcessor>",
            "<Url>/srv/ff/data/tuner.m3u</Url>",
            "<Path>/config/data/guide.xml</Path>",
        ] {
            assert!(xml.contains(expected), "{expected} in {xml}");
        }
        assert!(
            !xml.contains("DeviceId"),
            "a null member is left out: {xml}"
        );
        assert_eq!(
            export.outside,
            [("MovieRecordingPath".to_owned(), "/mnt/films".to_owned())]
        );
        assert_eq!(
            export.tuner_files,
            [("Attic".to_owned(), "/srv/ff/data/tuner.m3u".to_owned())]
        );
        assert_eq!(
            export.tuner_loopback,
            [("l".to_owned(), "http://127.0.0.1:5004".to_owned())]
        );
        assert_eq!(export.unreadable, ["a guide provider"]);
        // With nothing stored, the timer files are empty lists, not absent:
        // they replace what a source kept from before adoption.
        assert_eq!(file(&export, TIMERS_FILE), "[]");
        assert_eq!(file(&export, SERIES_TIMERS_FILE), "[]");
    }

    /// A loopback host is found in any spelling, and only its origin is kept:
    /// a user, a password or a query may be a credential.
    #[test]
    fn a_loopback_tuner_is_found_and_kept_to_its_origin() {
        for (url, origin) in [
            ("http://localhost/x.m3u", "http://localhost"),
            ("http://LOCALHOST:8080/x", "http://LOCALHOST:8080"),
            ("http://127.0.0.1/x?password=secret", "http://127.0.0.1"),
            (
                "https://user:pw@127.8.9.10:5004/x",
                "https://127.8.9.10:5004",
            ),
            ("http://[::1]:5004/x", "http://[::1]:5004"),
        ] {
            assert_eq!(loopback_origin(url).as_deref(), Some(origin), "{url}");
        }
        for url in [
            "http://tuner.lan/x",
            "http://192.168.1.5:5004",
            "/srv/x.m3u",
            "http://[fe80::1]/x",
        ] {
            assert_eq!(loopback_origin(url), None, "{url}");
        }
    }

    /// A timer with an empty external id keeps its own; a series timer whose
    /// settings do not parse, or that has no id at all, is reported.
    #[test]
    fn rows_without_a_usable_id_or_settings() {
        let dir = config_dir(&LiveTvOptions::default());
        let carried = CarriedRows {
            timers: vec![TimerRow {
                id: "internal".to_owned(),
                data: r#"{"ExternalId":""}"#.to_owned(),
                ..TimerRow::default()
            }],
            series_timers: vec![
                SeriesTimerRow {
                    data: "{".to_owned(),
                    ..SeriesTimerRow::default()
                },
                SeriesTimerRow {
                    data: r#"{"Name":"No id"}"#.to_owned(),
                    ..SeriesTimerRow::default()
                },
            ],
            ..CarriedRows::default()
        };
        let export =
            export_live_tv(dir.path(), &carried, &PathRewrite::default()).expect("exports");
        let timers: Vec<Value> = serde_json::from_str(file(&export, TIMERS_FILE)).expect("json");
        assert_eq!(timers[0]["Id"], "internal");
        assert_eq!(export.unreadable, ["a series timer", "a series timer"]);
        assert_eq!(file(&export, SERIES_TIMERS_FILE), "[]");
    }

    /// A recording outside every directory the export moves is listed by its
    /// folder, once.
    #[test]
    fn recordings_outside_the_data_directory_are_listed_by_folder() {
        let dir = config_dir(&LiveTvOptions::default());
        let completed = |id: &str, path: &str| TimerRow {
            id: id.to_owned(),
            data: r#"{"ExternalId":"x"}"#.to_owned(),
            recording: Some(RecordingRow {
                status: "Completed".to_owned(),
                path: Some(path.to_owned()),
                ..RecordingRow::default()
            }),
            ..TimerRow::default()
        };
        let carried = CarriedRows {
            timers: vec![
                completed("a", "/mnt/tv/Quiz Night/1.ts"),
                completed("b", "/mnt/tv/Quiz Night/2.ts"),
            ],
            ..CarriedRows::default()
        };
        let export =
            export_live_tv(dir.path(), &carried, &PathRewrite::default()).expect("exports");
        assert_eq!(
            export.outside,
            [(
                "Recordings folder".to_owned(),
                "/mnt/tv/Quiz Night".to_owned()
            )]
        );
        let timers: Vec<Value> = serde_json::from_str(file(&export, TIMERS_FILE)).expect("json");
        assert_eq!(timers[1]["RecordingPath"], "/mnt/tv/Quiz Night/2.ts");
    }
}
