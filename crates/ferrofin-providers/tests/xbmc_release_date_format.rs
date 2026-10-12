//! The NFO `ReleaseDateFormat` setting: custom .NET patterns are honored when
//! release dates are written and read, and a malformed pattern is refused.

use chrono::{DateTime, TimeZone, Utc};
use ferrofin_providers::container_types::MetadataResult;
use ferrofin_providers::xbmc::base_parser::NoDirectoryService;
use ferrofin_providers::xbmc::config::NfoConfiguration;
use ferrofin_providers::xbmc::item::{NfoBaseItem, NfoItemKind};
use ferrofin_providers::xbmc::saver::release_date_format_error;
use ferrofin_providers::xbmc::{StaticExternalIds, fetch_movie, new_result};
use ferrofin_providers::{save_artist, save_episode, save_movie};

fn at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
}

fn config(format: &str) -> NfoConfiguration {
    NfoConfiguration {
        release_date_format: format.to_owned(),
        ..NfoConfiguration::default()
    }
}

fn movie() -> MetadataResult<NfoBaseItem> {
    let mut result = new_result(NfoItemKind::Movie);
    result.item.name = Some("Dated".to_owned());
    result.item.premiere_date = Some(at(1999, 3, 31));
    result.item.end_date = Some(at(2001, 2, 3));
    result
}

fn read_back(xml: &str, config: &NfoConfiguration) -> MetadataResult<NfoBaseItem> {
    let mut result = new_result(NfoItemKind::Movie);
    fetch_movie(
        &mut result,
        "dated.nfo",
        xml,
        config,
        &StaticExternalIds::new(["Imdb"]),
        &NoDirectoryService,
    )
    .expect("fetch");
    result
}

#[test]
fn custom_patterns_are_written_and_read_back() {
    for (format, premiered, enddate) in [
        ("dd.MM.yyyy", "31.03.1999", "03.02.2001"),
        ("yyyy/M/d", "1999/3/31", "2001/2/3"),
        ("MMMM d, yyyy", "March 31, 1999", "February 3, 2001"),
        ("yyyyMMdd", "19990331", "20010203"),
    ] {
        let config = config(format);
        let xml = save_movie(&movie(), &config);
        for (tag, value) in [
            ("premiered", premiered),
            ("releasedate", premiered),
            ("enddate", enddate),
        ] {
            assert!(
                xml.contains(&format!("<{tag}>{value}</{tag}>")),
                "{format}: {tag}\n{xml}"
            );
        }
        let item = read_back(&xml, &config).item;
        assert_eq!(item.premiere_date, Some(at(1999, 3, 31)), "{format}");
        assert_eq!(item.end_date, Some(at(2001, 2, 3)), "{format}");
        // The default pattern does not match what a custom one wrote.
        assert_eq!(
            read_back(&xml, &NfoConfiguration::default())
                .item
                .premiere_date,
            None,
            "{format}"
        );
    }
}

#[test]
fn episode_and_artist_dates_use_the_pattern() {
    let config = config("dd.MM.yyyy");
    let mut episode = new_result(NfoItemKind::Episode);
    episode.item.premiere_date = Some(at(2010, 12, 5));
    assert!(save_episode(&episode, &config).contains("<aired>05.12.2010</aired>"));

    let mut artist = new_result(NfoItemKind::MusicArtist);
    artist.item.premiere_date = Some(at(1970, 1, 2));
    artist.item.end_date = Some(at(1982, 6, 7));
    let xml = save_artist(&artist, &[], &config);
    assert!(xml.contains("<formed>02.01.1970</formed>"), "{xml}");
    assert!(xml.contains("<disbanded>07.06.1982</disbanded>"), "{xml}");
}

#[test]
fn dates_must_match_the_pattern_exactly() {
    let default = NfoConfiguration::default();
    for padded in [
        "<premiered> 1999-03-31 </premiered>",
        "<premiered>1999-3-31</premiered>",
        "<premiered>1999-03-31 00:00:00</premiered>",
    ] {
        let xml = format!("<movie>{padded}</movie>");
        assert_eq!(
            read_back(&xml, &default).item.premiere_date,
            None,
            "{padded}"
        );
    }
    let xml = "<movie><premiered>1999-03-31</premiered></movie>";
    assert_eq!(
        read_back(xml, &default).item.premiere_date,
        Some(at(1999, 3, 31))
    );
}

#[test]
fn a_malformed_pattern_is_refused_only_when_a_date_would_be_written() {
    let mut dated = movie().item;
    let mut undated = dated.clone();
    undated.premiere_date = None;
    undated.end_date = None;
    for format in ["H", "%", "'unterminated", "ffffffff"] {
        let config = config(format);
        assert!(
            release_date_format_error(&dated, &config).is_some(),
            "{format}"
        );
        assert!(
            release_date_format_error(&undated, &config).is_none(),
            "{format}"
        );
    }
    assert!(release_date_format_error(&dated, &NfoConfiguration::default()).is_none());
    // An episode's end date is never written.
    dated.kind = NfoItemKind::Episode;
    dated.premiere_date = None;
    assert!(release_date_format_error(&dated, &config("H")).is_none());
}
