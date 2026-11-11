//! What the guide refresh stores from a listing beyond the airing's own
//! columns: the `ProgramEtag` provider id `GuideManager.GetProgram` keeps on
//! each programme item (gl-763).

use ferrofin_model::live_tv::ListingsProviderInfo;

use crate::manager::{CategoryClasses, xmltv_etag};

/// A guide whose airings reach every field the XMLTV etag hashes: a series
/// episode, a movie with episode fields to clear, ratings, a star rating
/// (which Jellyfin never parses into a value), artwork, a year, kids, news and
/// sports categories, the premiere, live, new and repeat flags, non-ASCII text,
/// a `+0100` start and a `dd_progid` programme id, which becomes the
/// `ShowId`.
///
/// The movie's backdrop `<image>` and its `<rating>` each start a new line on
/// purpose. `Jellyfin.XmlTv` reads an `<image>` (or `<url>`) past its end tag
/// and its caller then skips one more node, so the element right after it is
/// dropped when no whitespace comes between them: on one line, Jellyfin stored
/// no backdrop, and with only the backdrop moved, no rating. Ferrofin reads
/// both, so on such a compact guide the airing's stored rating or artwork and
/// its etag differ from Jellyfin's (a Jellyfin bug not ported).
const ORACLE_GUIDE: &str = r#"
    <tv><channel id="one.tv"><display-name>Channel One</display-name></channel><channel id="two.tv"><display-name>Channel Two</display-name></channel>
    <programme start="20261111050000 +0000" stop="20261111060000 +0000" channel="one.tv"><title>Plain Show</title><desc>Just a show.</desc></programme>
    <programme start="20261111060000 +0000" stop="20261111070000 +0000" channel="one.tv"><title>Harbour Lights</title><sub-title>The Storm</sub-title><desc>Episode desc.</desc><category>Drama</category><category>  </category><episode-num system="xmltv_ns">1.4.</episode-num><previously-shown/><rating system="VCHIP"><value>TV-PG</value></rating></programme>
    <programme start="20261111070000 +0000" stop="20261111090000 +0000" channel="one.tv"><title>Night Heist</title><sub-title>ignored for movies</sub-title><desc>A movie.</desc><date>2019</date><category>Movie</category><category>Thriller</category><episode-num system="xmltv_ns">0.2.</episode-num><icon src="http://img.test/poster.png"/><image type="still">http://img.test/still.jpg</image>
    <image type="backdrop">http://img.test/back.jpg</image>
    <rating system="MPAA"><value>PG-13</value></rating><star-rating><value>7/10</value></star-rating></programme>
    <programme start="20261111090000 +0000" stop="20261111100000 +0000" channel="one.tv"><title>Café – 東京 Nights</title><desc>Unicode 🎬 desc with é.</desc><category>Kids</category><category>News</category><category>Sports</category><premiere/><live/><new/></programme>
    <programme start="20261111060000 +0100" stop="20261111070000 +0100" channel="two.tv"><title>Offset Hour</title><episode-num system="xmltv_ns">.7.</episode-num><star-rating><value>8</value></star-rating></programme>
    <programme start="20261111060000 +0000" stop="20261111070000 +0000" channel="two.tv"><title>Series No Number</title><sub-title>Pilot</sub-title><category>Series</category></programme>
    <programme start="20261111080000 +0000" stop="20261111090000 +0000" channel="two.tv"><title>Progid Show</title><sub-title>Ep</sub-title><episode-num system="dd_progid">EP00003026.0666</episode-num><episode-num system="xmltv_ns">2.5.</episode-num></programme>
    </tv>
"#;

/// The `ProgramEtag` provider id Jellyfin 12.4 stored for each airing of
/// [`ORACLE_GUIDE`] (the official image, an M3U tuner on the guide's channels,
/// one "Refresh Guide" run).
const ORACLE_ETAGS: [(&str, &str); 7] = [
    (
        "Plain Show",
        "xmltv-sha256-v1:372EE78F9C1F606B19BF6EE95BC59DF6280383DC4FB0B836E2F8985D675D0525",
    ),
    (
        "Harbour Lights",
        "xmltv-sha256-v1:05516AC365AFBB8B55C116EF77A9F4547957D0194069864A703E2BF1BE1FEF48",
    ),
    (
        "Night Heist",
        "xmltv-sha256-v1:BBA35A606F38B8DCB538E9A7067B885E54335E731532DC88351848401EDDC7EC",
    ),
    (
        "Café – 東京 Nights",
        "xmltv-sha256-v1:6A5431DFDA9123D6B1D1A6337D8FA4577C9600B2C363277C433AA5C50CC9BC14",
    ),
    (
        "Offset Hour",
        "xmltv-sha256-v1:B99DB2A6CB73A6A798ABDDB509671314FFBE59E5FD5DFD1D7125FC91B02220C6",
    ),
    (
        "Progid Show",
        "xmltv-sha256-v1:9E2EF2941B54855B58AC812408D38C8F72C6800441396841D9F25BABA485D895",
    ),
    (
        "Series No Number",
        "xmltv-sha256-v1:2B20E99579B59B679345C8B63408D03A7B9635A9AB2FF7A22560F0EAC23F7BA9",
    ),
];

/// Each airing's etag is the one Jellyfin 12.4 computes for it, so a
/// database Ferrofin wrote goes back to Jellyfin with every programme on its
/// fast path.
#[test]
fn xmltv_etags_match_jellyfin() {
    let guide = crate::xmltv::parse_xmltv(ORACLE_GUIDE);
    let classes = CategoryClasses::from_provider(&ListingsProviderInfo::default());
    let mut etags: Vec<(String, Option<String>)> = guide
        .programmes
        .iter()
        .map(|prog| {
            (
                prog.title.clone(),
                xmltv_etag(prog, &classes.classify(prog)),
            )
        })
        .collect();
    let mut expected: Vec<(String, Option<String>)> = ORACLE_ETAGS
        .iter()
        .map(|(title, etag)| ((*title).to_owned(), Some((*etag).to_owned())))
        .collect();
    etags.sort();
    expected.sort();
    assert_eq!(etags, expected);
}

/// gl-763: the guide refresh stores each XMLTV programme's etag as its
/// `ProgramEtag` provider id. `GetProgram` replaces the ids with the listing's
/// when it saves the programme, so an unchanged listing leaves an editor's
/// extra id alone and a changed one replaces the set. (The DTO service
/// projects the id; that is checked over HTTP, the lab's DTO service being a
/// fake.)
#[tokio::test]
async fn the_guide_refresh_stores_each_programmes_etag() {
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::options::InternalItemsQuery;
    use ferrofin_traits::stubs::LiveTvManager as _;

    let day = super::guide_day(0);
    let guide = |title: &str| {
        format!(
            "<tv><channel id=\"one.tv\"><display-name>Channel One</display-name></channel>\
             <programme start=\"{day}060000 +0000\" stop=\"{day}070000 +0000\" channel=\"one.tv\">\
             <title>{title}</title></programme></tv>"
        )
    };
    let etag_of = |xml: &str| {
        let guide = crate::xmltv::parse_xmltv(xml);
        let classes = CategoryClasses::from_provider(&ListingsProviderInfo::default());
        let prog = &guide.programmes[0];
        xmltv_etag(prog, &classes.classify(prog)).expect("etag")
    };
    let lab = super::item_lab(super::M3U, Some(guide("Morning"))).await;
    lab.mgr.refresh_guide().await.expect("refresh");

    let programme = lab
        .items
        .get_item_list(&InternalItemsQuery {
            include_item_types: vec![BaseItemKind::LiveTvProgram],
            ..InternalItemsQuery::default()
        })
        .await
        .expect("programmes");
    let [programme] = &programme[..] else {
        panic!("one programme: {programme:?}");
    };
    let id = uuid::Uuid::parse_str(&programme.id).expect("guid");
    let ids = || async {
        let mut ids = lab
            .persistence
            .provider_ids_for_items(&[id])
            .await
            .expect("ids")
            .remove(&id)
            .unwrap_or_default();
        ids.sort();
        ids
    };
    let morning = etag_of(&guide("Morning"));
    assert_eq!(ids().await, [("ProgramEtag".to_owned(), morning.clone())]);

    // An editor's id survives a refresh of the same listing.
    lab.persistence
        .save_provider_id(id, "Custom", "kept")
        .await
        .expect("an editor's id");
    lab.mgr.refresh_guide().await.expect("second refresh");
    assert_eq!(
        ids().await,
        [
            ("Custom".to_owned(), "kept".to_owned()),
            ("ProgramEtag".to_owned(), morning),
        ]
    );

    // A changed listing replaces the set.
    lab.sources
        .0
        .lock()
        .expect("sources")
        .insert("http://guide/xmltv.xml".to_owned(), guide("Evening"));
    let providers = lab.mgr.get_listing_providers().await.expect("providers");
    let provider = providers[0].id.as_deref().expect("provider id");
    tokio::fs::remove_file(lab.mgr.xmltv.cache_file_for_test(provider))
        .await
        .expect("drop the cached guide");
    lab.mgr.refresh_guide().await.expect("third refresh");
    assert_eq!(
        ids().await,
        [("ProgramEtag".to_owned(), etag_of(&guide("Evening")))]
    );
}
