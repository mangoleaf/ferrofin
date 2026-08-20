//! `album.nfo` / `artist.nfo` round trips — the music half of the XbmcMetadata
//! port.
//!
//! C# reads both through the plain `BaseNfoParser<T>` (neither has custom
//! elements) and writes them with `AlbumNfoSaver` / `ArtistNfoSaver`, which add
//! the artist/album-artist/track and disbanded/album blocks on top of the common
//! nodes. The oracle here is the same as the video savers': parse → save →
//! re-parse, plus explicit assertions on the blocks only the savers emit.

use chrono::TimeZone;
use ferrofin_providers::xbmc::base_parser::NoDirectoryService;
use ferrofin_providers::xbmc::config::NfoConfiguration;
use ferrofin_providers::xbmc::item::NfoItemKind;
use ferrofin_providers::xbmc::{StaticExternalIds, fetch_music, new_result};
use ferrofin_providers::{NfoAlbum, NfoTrack, save_album, save_artist};

/// Parses `xml` as the given music kind.
fn parse(
    kind: NfoItemKind,
    xml: &str,
) -> ferrofin_providers::container_types::MetadataResult<ferrofin_providers::xbmc::item::NfoBaseItem>
{
    let mut result = new_result(kind);
    fetch_music(
        &mut result,
        "/music/album.nfo",
        xml,
        &NfoConfiguration::default(),
        &StaticExternalIds::new(["MusicBrainzAlbum", "MusicBrainzArtist"]),
        &NoDirectoryService,
    )
    .expect("parse");
    result
}

#[test]
fn an_album_nfo_parses_its_common_nodes() {
    let result = parse(
        NfoItemKind::MusicAlbum,
        r"<album>
            <title>OK Computer</title>
            <year>1997</year>
            <rating>9.2</rating>
            <plot>Third studio album.</plot>
            <genre>Alternative Rock</genre>
            <musicbrainzalbumid>b1392450-e666-3926-a536-22c65998f3d7</musicbrainzalbumid>
          </album>",
    );
    let item = &result.item;
    assert_eq!(item.name.as_deref(), Some("OK Computer"));
    assert_eq!(item.production_year, Some(1997));
    assert_eq!(item.overview.as_deref(), Some("Third studio album."));
    assert_eq!(item.genres, ["Alternative Rock"]);
    assert_eq!(
        item.provider_ids
            .get("MusicBrainzAlbum")
            .map(String::as_str),
        Some("b1392450-e666-3926-a536-22c65998f3d7")
    );
}

#[test]
fn the_album_saver_writes_artists_album_artists_and_ordered_tracks() {
    let mut result = new_result(NfoItemKind::MusicAlbum);
    result.item.name = Some("OK Computer".into());
    result.item.artists = vec!["Radiohead".into(), "  ".into()];
    result.item.album_artists = vec!["Radiohead".into()];
    // Deliberately out of order: disc 2 first, then disc 1 positions 2 and 1.
    let tracks = vec![
        NfoTrack {
            disc: Some(2),
            position: Some(1),
            title: Some("Lull".into()),
            run_time_ticks: Some(2 * 60 * 10_000_000),
            ..NfoTrack::default()
        },
        NfoTrack {
            disc: Some(1),
            position: Some(2),
            title: Some("Paranoid Android".into()),
            run_time_ticks: Some((6 * 60 + 23) * 10_000_000),
            ..NfoTrack::default()
        },
        NfoTrack {
            disc: Some(1),
            position: Some(1),
            title: Some("Airbag".into()),
            run_time_ticks: Some((4 * 60 + 44) * 10_000_000),
            ..NfoTrack::default()
        },
    ];
    let xml = save_album(&result, &tracks, &NfoConfiguration::default());

    assert!(xml.contains("<artist>Radiohead</artist>"));
    assert!(xml.contains("<albumartist>Radiohead</albumartist>"));
    // Ordered by disc, then position.
    let airbag = xml.find("Airbag").expect("airbag");
    let paranoid = xml.find("Paranoid Android").expect("paranoid");
    let lull = xml.find("Lull").expect("lull");
    assert!(airbag < paranoid && paranoid < lull, "disc/position order");
    // Durations are mm:ss.
    assert!(xml.contains("<duration>04:44</duration>"));
    assert!(xml.contains("<duration>06:23</duration>"));
    assert!(xml.contains("<disc>2</disc>"));
    // A blank artist entry is dropped, not written as an empty tag.
    assert_eq!(xml.matches("<artist>").count(), 1);
}

#[test]
fn a_zero_disc_or_position_is_omitted() {
    // C# writes neither tag when the value is absent or zero.
    let result = new_result(NfoItemKind::MusicAlbum);
    let tracks = vec![NfoTrack {
        disc: Some(0),
        position: None,
        title: Some("Untitled".into()),
        ..NfoTrack::default()
    }];
    let xml = save_album(&result, &tracks, &NfoConfiguration::default());
    assert!(!xml.contains("<disc>"));
    assert!(!xml.contains("<position>"));
    assert!(xml.contains("<title>Untitled</title>"));
}

#[test]
fn the_artist_saver_writes_disbanded_and_albums_by_year() {
    let mut result = new_result(NfoItemKind::MusicArtist);
    result.item.name = Some("Radiohead".into());
    result.item.end_date = Some(chrono::Utc.with_ymd_and_hms(2011, 3, 4, 0, 0, 0).unwrap());
    let albums = vec![
        NfoAlbum {
            title: Some("In Rainbows".into()),
            year: Some(2007),
            ..NfoAlbum::default()
        },
        NfoAlbum {
            title: Some("The Bends".into()),
            year: Some(1995),
            ..NfoAlbum::default()
        },
    ];
    let xml = save_artist(&result, &albums, &NfoConfiguration::default());

    assert!(xml.contains("<disbanded>2011-03-04</disbanded>"), "{xml}");
    let bends = xml.find("The Bends").expect("bends");
    let rainbows = xml.find("In Rainbows").expect("rainbows");
    assert!(bends < rainbows, "albums are ordered by year");
    assert!(xml.contains("<year>1995</year>"));
}

#[test]
fn an_album_survives_a_save_and_reparse() {
    let first = parse(
        NfoItemKind::MusicAlbum,
        r"<album>
            <title>Kid A</title>
            <year>2000</year>
            <plot>Fourth studio album.</plot>
            <genre>Electronic</genre>
          </album>",
    );
    let xml = save_album(&first, &[], &NfoConfiguration::default());
    let second = parse(NfoItemKind::MusicAlbum, &xml);
    assert_eq!(second.item.name, first.item.name);
    assert_eq!(second.item.production_year, first.item.production_year);
    assert_eq!(second.item.overview, first.item.overview);
    assert_eq!(second.item.genres, first.item.genres);
}
