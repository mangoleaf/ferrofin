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

/// gl-1631: a guide whose text is padded, empty or blank, which
/// `Jellyfin.XmlTv` keeps as written (`ReadElementContentAsString`). Not
/// indented: the padding inside its elements is the point.
const PADDED_GUIDE: &str = r#"
<tv><channel id="one.tv"><display-name>Channel One</display-name></channel><channel id="two.tv"><display-name>Channel Two</display-name></channel>
<programme start="20261111070000 +0000" stop="20261111080000 +0000" channel="one.tv"><title>  Padded Show 
</title><desc>
      A padded description.
    </desc><category> Drama </category><category>   </category><rating system="VCHIP"><value> TV-PG </value></rating></programme>
<programme start="20261111080000 +0000" stop="20261111090000 +0000" channel="one.tv"><title>Empty Parts</title><sub-title></sub-title><desc/><episode-num system="xmltv_ns">0.3.</episode-num><rating system="MPAA"><value>   </value></rating></programme>
<programme start="20261111090000 +0000" stop="20261111100000 +0000" channel="two.tv"><title>Padded Ids</title><sub-title> The Pilot </sub-title><episode-num system="dd_progid"> EP00001111.0001 </episode-num>
<image type="still"> http://img.test/still.jpg </image>
</programme>
</tv>
"#;

/// The `ProgramEtag` Jellyfin 12.4 stored for each airing of [`PADDED_GUIDE`].
const PADDED_ETAGS: [(&str, &str); 3] = [
    (
        "  Padded Show \n",
        "xmltv-sha256-v1:9128DD42F50BBC29F77C546FAD1946D33AB2C1EA10DC6654F10D0243980B6A4D",
    ),
    (
        "Empty Parts",
        "xmltv-sha256-v1:D02F3FCA19702BD1EFF5066B9AAB9E123D375ED041DACE546AA02831CFFB17BF",
    ),
    (
        "Padded Ids",
        "xmltv-sha256-v1:E230FB4A925EA3DA4722EEFE280B5F6CF1378C40C0ABE260B9F6DD9813C81F89",
    ),
];

/// A programme's text is kept as the guide writes it: padding stays, an empty
/// `<desc/>` or `<sub-title/>` is `""`, a blank category is no genre and a
/// blank rating value is a rating. The etags are Jellyfin's.
#[test]
fn programme_text_is_kept_as_written() {
    let guide = crate::xmltv::parse_xmltv(PADDED_GUIDE);
    let classes = CategoryClasses::from_provider(&ListingsProviderInfo::default());
    let by_title = |title: &str| {
        guide
            .programmes
            .iter()
            .find(|p| p.title == title)
            .expect(title)
    };
    for (title, expected) in PADDED_ETAGS {
        let prog = by_title(title);
        assert_eq!(
            xmltv_etag(prog, &classes.classify(prog)).as_deref(),
            Some(expected),
            "{title:?}"
        );
    }
    let padded = by_title("  Padded Show \n");
    assert_eq!(
        padded.desc.as_deref(),
        Some("\n      A padded description.\n    ")
    );
    assert_eq!(padded.categories, [" Drama "]);
    assert_eq!(padded.rating.as_deref(), Some(" TV-PG "));
    let empty = by_title("Empty Parts");
    assert_eq!(
        (empty.desc.as_deref(), empty.sub_title.as_deref()),
        (Some(""), Some(""))
    );
    assert_eq!(empty.rating.as_deref(), Some("   "));
    let ids = by_title("Padded Ids");
    assert_eq!(ids.program_id.as_deref(), Some(" EP00001111.0001 "));
    assert_eq!(
        ids.still_image.as_deref(),
        Some(" http://img.test/still.jpg ")
    );
}

/// gl-1631: what a guide's text makes of an airing's kind and ids. An empty
/// `<sub-title/>` is no episode title (`!string.IsNullOrEmpty`), so with no
/// episode number the airing is no series; a padded category matches no
/// category list; a missing `<title>` is null in the etag and gives no series
/// id, where an empty one is `""` and still hashes.
#[test]
fn a_guides_text_decides_the_kind_as_upstream() {
    let classes = CategoryClasses::from_provider(&ListingsProviderInfo::default());
    let one = |children: &str| {
        let guide = crate::xmltv::parse_xmltv(&format!(
            "<tv><programme start=\"20260725060000 +0000\" stop=\"20260725070000 +0000\" \
             channel=\"one.tv\">{children}</programme></tv>"
        ));
        guide.programmes.into_iter().next().expect("one programme")
    };
    let special = one("<title>Special</title><sub-title/>");
    let class = classes.classify(&special);
    assert_eq!(class.episode_title.as_deref(), Some(""));
    assert!(!class.is_series);

    let padded = one("<title>Film</title><category> Movie </category>");
    assert!(
        !classes.classify(&padded).is_movie,
        "\" Movie \" is no movie category"
    );

    let numbered = r#"<episode-num system="xmltv_ns">0.1.</episode-num>"#;
    let empty = one(&format!("<title></title>{numbered}"));
    let missing = one(numbered);
    assert!(missing.missing_title && !empty.missing_title);
    let (empty_class, missing_class) = (classes.classify(&empty), classes.classify(&missing));
    assert!(empty_class.external_series_id.is_some(), "md5(\"\")");
    assert_eq!(missing_class.external_series_id, None);
    assert_ne!(
        xmltv_etag(&empty, &empty_class),
        xmltv_etag(&missing, &missing_class),
        "Name hashes \"\" for an empty title and null for none"
    );
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
