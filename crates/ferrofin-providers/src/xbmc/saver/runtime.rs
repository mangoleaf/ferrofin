//! Runtime fields and existing custom XML, ported from BaseNfoSaver.
use super::{NfoBaseItem, NfoItemKind, NfoWriter};
use ferrofin_db::entities::base_items::MediaStreamInfoEntity;
use ferrofin_model::{dto::UserItemDataDto, entities::ImageType};
use ferrofin_traits::{error::ServiceError, options::ItemImageInfo};
use quick_xml::{Reader, events::Event};

#[derive(Clone, Copy)]
pub(crate) struct DocumentExtras<'a> {
    pub item: &'a NfoBaseItem,
    pub original_language: Option<&'a str>,
    pub images: &'a [ItemImageInfo],
    pub streams: &'a [MediaStreamInfoEntity],
    pub user: Option<&'a UserItemDataDto>,
    pub save_images: bool,
    pub existing: Option<&'a str>,
}

pub(crate) fn complete_document(
    mut xml: String,
    extras: DocumentExtras<'_>,
) -> Result<String, ServiceError> {
    let mut writer = NfoWriter {
        buf: String::new(),
        depth: 1,
    };
    if let Some(language) = extras.original_language.filter(|value| !value.is_empty()) {
        writer.element("originallanguage", language);
    }
    if extras.save_images {
        add_images(&mut writer, extras.images);
    }
    if let Some(user) = extras.user {
        add_user(&mut writer, extras.item, user);
    }
    if extras.item.kind.is_video() {
        add_streams(&mut writer, extras.item, extras.streams);
    }
    if let Some(existing) = extras.existing {
        add_custom(&mut writer, extras.item, existing);
    }
    let end = xml
        .rfind("</")
        .ok_or_else(|| ServiceError::backend("NFO serializer omitted root element"))?;
    xml.insert_str(end, &writer.finish());
    Ok(xml)
}

fn add_images(writer: &mut NfoWriter, images: &[ItemImageInfo]) {
    writer.start_element("art");
    if let Some(image) = images
        .iter()
        .find(|image| image.image_type == ImageType::Primary)
    {
        writer.element("poster", &image.path);
    }
    let mut backdrops: Vec<_> = images
        .iter()
        .filter(|image| image.image_type == ImageType::Backdrop)
        .collect();
    backdrops.sort_by_key(|image| image.path.trim());
    for image in backdrops {
        writer.element("fanart", &image.path);
    }
    writer.end_element("art");
}

fn seconds(ticks: i64) -> String {
    // Avoid floating point loss for large tick values; retain subsecond resume positions.
    let whole = ticks / 10_000_000;
    let remainder = ticks.unsigned_abs() % 10_000_000;
    if remainder == 0 {
        return whole.to_string();
    }
    format!(
        "{}{whole}.{remainder:07}",
        if ticks < 0 && whole == 0 { "-" } else { "" }
    )
    .trim_end_matches('0')
    .to_owned()
}

fn add_user(writer: &mut NfoWriter, item: &NfoBaseItem, user: &UserItemDataDto) {
    writer.element("isuserfavorite", &user.is_favorite.to_string());
    if let Some(rating) = user.rating {
        writer.element("userrating", &rating.to_string());
    }
    writer.element("playcount", &user.play_count.to_string());
    writer.element("watched", &user.played.to_string());
    if let Some(date) = user.last_played_date {
        writer.element(
            "lastplayed",
            &date.format(super::DATE_ADDED_FORMAT).to_string(),
        );
    }
    writer.start_element("resume");
    writer.element("position", &seconds(user.playback_position_ticks));
    writer.element("total", &seconds(item.run_time_ticks.unwrap_or_default()));
    writer.end_element("resume");
}

fn add_streams(writer: &mut NfoWriter, item: &NfoBaseItem, streams: &[MediaStreamInfoEntity]) {
    writer.start_element("fileinfo");
    writer.start_element("streamdetails");
    for stream in streams {
        let name = match stream.stream_type {
            0 => "audio",
            1 => "video",
            2 => "subtitle",
            3 => "embeddedimage",
            4 => "data",
            5 => "lyric",
            _ => continue,
        };
        writer.start_element(name);
        if let Some(codec) = stream.codec.as_deref().filter(|codec| !codec.is_empty()) {
            let tag = stream
                .codec_tag
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let codec = if tag.contains("xvid") {
                "xvid"
            } else if tag.contains("divx") {
                "divx"
            } else {
                codec
            };
            writer.element("codec", codec);
            writer.element("micodec", codec);
        }
        for (name, value) in [
            ("bitrate", stream.bit_rate),
            ("width", stream.width),
            ("height", stream.height),
            ("channels", stream.channels),
            ("samplingrate", stream.sample_rate),
        ] {
            if let Some(value) = value {
                writer.element(name, &value.to_string());
            }
        }
        if let Some(aspect) = stream
            .aspect_ratio
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            writer.element("aspect", aspect);
            writer.element("aspectratio", aspect);
        }
        if let Some(rate) = stream
            .average_frame_rate
            .filter(|rate| *rate < 1000.0)
            .or(stream.real_frame_rate)
        {
            writer.element("framerate", &rate.to_string());
        }
        if let Some(language) = stream.language.as_deref().filter(|value| !value.is_empty()) {
            let language: String = language.chars().filter(|c| !matches!(c, '\x00'..='\x08'|'\x0b'|'\x0c'|'\x0e'..='\x1f'|'\x7f'..='\u{9f}'|'\u{feff}'|'\u{fffe}'|'\u{ffff}')).collect();
            writer.element("language", &language);
        }
        writer.element(
            "scantype",
            if stream.is_interlaced == Some(true) {
                "interlaced"
            } else {
                "progressive"
            },
        );
        writer.element("default", if stream.is_default { "True" } else { "False" });
        writer.element("forced", if stream.is_forced { "True" } else { "False" });
        if stream.is_original {
            writer.element("original", "True");
        }
        if stream.stream_type == 1 {
            add_video_info(writer, item);
        }
        writer.end_element(name);
    }
    writer.end_element("streamdetails");
    writer.end_element("fileinfo");
}

fn add_video_info(writer: &mut NfoWriter, item: &NfoBaseItem) {
    if let Some(ticks) = item.run_time_ticks {
        writer.element(
            "duration",
            &ticks.div_euclid(super::TICKS_PER_MINUTE).to_string(),
        );
        writer.element(
            "durationinseconds",
            &ticks.div_euclid(10_000_000).to_string(),
        );
    }
    if let Some(format) = item.video_3d_format.filter(|format| {
        !matches!(
            format,
            ferrofin_model::entities::Video3DFormat::Unrecognized(_)
        )
    }) {
        use ferrofin_model::entities::Video3DFormat;
        writer.element(
            "format3d",
            match format {
                Video3DFormat::FullSideBySide => "FSBS",
                Video3DFormat::FullTopAndBottom => "FTAB",
                Video3DFormat::HalfSideBySide => "HSBS",
                Video3DFormat::HalfTopAndBottom => "HTAB",
                Video3DFormat::Mvc => "MVC",
                Video3DFormat::Unrecognized(_) => unreachable!("filtered above"),
            },
        );
    }
}

const COMMON_TAGS: &str = "plot customrating lockdata dateadded title rating year sorttitle mpaa aspectratio collectionnumber tmdbid rottentomatoesid language tvcomid tagline studio genre tag runtime actor criticrating fileinfo director writer trailer premiered releasedate outline id credits originaltitle originallanguage watched playcount lastplayed art resume biography formed review style imdbid imdb_id country audiodbalbumid audiodbartistid enddate lockedfields zap2itid tvrageid musicbrainzartistid musicbrainzalbumartistid musicbrainzalbumid musicbrainzreleasegroupid tvdbid collectionitem isuserfavorite userrating countrycode";

fn owned_tag(item: &NfoBaseItem, tag: &str) -> bool {
    let tag = tag.to_ascii_lowercase();
    let specific = match item.kind {
        NfoItemKind::Episode => {
            "aired season episode episodenumberend airsafter_season airsbefore_episode airsbefore_season displayseason displayepisode showtitle"
        }
        NfoItemKind::Series => "id episodeguide season episode status displayorder",
        NfoItemKind::Season => "seasonnumber",
        NfoItemKind::MusicAlbum => "track artist albumartist",
        NfoItemKind::MusicArtist => "album disbanded",
        _ => "album artist set id",
    };
    COMMON_TAGS
        .split_whitespace()
        .chain(specific.split_whitespace())
        .any(|owned| owned == tag)
        || item
            .provider_ids
            .keys()
            .any(|key| format!("{}id", key.to_ascii_lowercase()) == tag)
}

fn add_custom(writer: &mut NfoWriter, item: &NfoBaseItem, existing: &str) {
    let mut reader = Reader::from_str(existing);
    let mut depth = 0;
    let mut capture = None;
    let mut namespaces = Vec::new();
    loop {
        let start = usize::try_from(reader.buffer_position()).unwrap_or(existing.len());
        match reader.read_event() {
            Ok(Event::Start(node)) => {
                if depth == 0 {
                    namespaces = node
                        .attributes()
                        .flatten()
                        .filter_map(|attribute| {
                            let key = attribute.key.as_ref();
                            if key != "xmlns" && !key.starts_with("xmlns:") {
                                return None;
                            }
                            let value = attribute
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .ok()?;
                            Some((key.to_owned(), value.into_owned()))
                        })
                        .collect();
                }
                if depth == 1 && !owned_tag(item, node.name().as_ref()) {
                    capture = Some(start);
                }
                depth += 1;
            }
            Ok(Event::Empty(node)) if depth == 1 && !owned_tag(item, node.name().as_ref()) => {
                append_custom(
                    writer,
                    existing,
                    start,
                    reader.buffer_position(),
                    &namespaces,
                );
            }
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 1
                    && let Some(start) = capture.take()
                {
                    append_custom(
                        writer,
                        existing,
                        start,
                        reader.buffer_position(),
                        &namespaces,
                    );
                }
            }
            Ok(Event::DocType(_)) => {
                // XmlReader's default DtdProcessing.Prohibit refuses these files.
                tracing::warn!("not importing custom NFO tags from a document with a DTD");
                break;
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                tracing::warn!(%error, "could not finish reading custom NFO tags");
                break;
            }
            _ => {}
        }
    }
}

fn append_custom(
    writer: &mut NfoWriter,
    existing: &str,
    start: usize,
    end: u64,
    namespaces: &[(String, String)],
) {
    let Some(fragment) = usize::try_from(end)
        .ok()
        .and_then(|end| existing.get(start..end))
    else {
        return;
    };
    let mut reader = Reader::from_str(fragment);
    let Ok(Event::Start(node) | Event::Empty(node)) = reader.read_event() else {
        return;
    };
    // Namespace declarations inherited from the old root must travel with the
    // custom child. Preserve a child's own declaration when it shadows one.
    let own: Vec<_> = node
        .attributes()
        .flatten()
        .map(|attribute| attribute.key.as_ref().to_owned())
        .collect();
    let name_end = 1 + node.name().as_ref().len();
    writer.indent();
    writer.buf.push_str(&fragment[..name_end]);
    for (name, value) in namespaces {
        if !own.contains(name) {
            use std::fmt::Write as _;
            let _ = write!(writer.buf, " {name}=\"{}\"", super::escape_attr(value));
        }
    }
    writer.buf.push_str(&fragment[name_end..]);
    writer.buf.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_types::MetadataResult;

    #[test]
    fn custom_nested_tags_survive_but_removed_owned_fields_do_not() {
        let mut item = NfoBaseItem {
            kind: NfoItemKind::Movie,
            name: Some("New & better".into()),
            ..Default::default()
        };
        item.set_provider_id("CustomProvider", "updated");
        let result = MetadataResult {
            item,
            ..Default::default()
        };
        let old = r#"<movie><TITLE>stale</TITLE><genre>removed</genre><art><poster>old</poster></art><CustomProviderId>stale</CustomProviderId><custom flag="yes"><nested><![CDATA[keep <me>]]></nested><empty /></custom><setting enabled="true" /></movie>"#;
        let xml = complete_document(
            super::super::save_movie(&result, &crate::xbmc::config::NfoConfiguration::default()),
            DocumentExtras {
                item: &result.item,
                original_language: Some("fr"),
                images: &[],
                streams: &[],
                user: None,
                save_images: false,
                existing: Some(old),
            },
        )
        .unwrap();
        assert!(xml.contains("<title>New &amp; better</title>"));
        assert!(xml.contains(
            r#"<custom flag="yes"><nested><![CDATA[keep <me>]]></nested><empty /></custom>"#
        ));
        assert!(xml.contains(r#"<setting enabled="true" />"#));
        assert!(xml.contains("<originallanguage>fr</originallanguage>"));
        assert!(!xml.contains("stale"));
        assert!(!xml.contains("removed"));
        assert!(!xml.contains("<art>"));
        let again = complete_document(
            super::super::save_movie(&result, &crate::xbmc::config::NfoConfiguration::default()),
            DocumentExtras {
                item: &result.item,
                original_language: Some("fr"),
                images: &[],
                streams: &[],
                user: None,
                save_images: false,
                existing: Some(&xml),
            },
        )
        .unwrap();
        assert_eq!(again, xml);
    }

    #[test]
    fn custom_tags_keep_inherited_namespaces_and_ignore_documents_with_dtds() {
        let item = NfoBaseItem {
            kind: NfoItemKind::Movie,
            ..Default::default()
        };
        let mut writer = NfoWriter {
            buf: String::new(),
            depth: 0,
        };
        add_custom(
            &mut writer,
            &item,
            r#"<movie xmlns:ext="urn:custom" xmlns="urn:default"><ext:custom>value</ext:custom><extra xmlns="urn:own" /></movie>"#,
        );
        let xml = writer.finish();
        assert!(xml.contains(
            r#"<ext:custom xmlns:ext="urn:custom" xmlns="urn:default">value</ext:custom>"#
        ));
        assert!(xml.contains(r#"<extra xmlns:ext="urn:custom" xmlns="urn:own" />"#));
        let mut writer = NfoWriter {
            buf: String::new(),
            depth: 0,
        };
        add_custom(
            &mut writer,
            &item,
            r#"<!DOCTYPE movie [<!ENTITY custom "value">]><movie><extra>&custom;</extra></movie>"#,
        );
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn media_fields_include_codec_override_frame_rate_flags_and_duration() {
        let item = NfoBaseItem {
            kind: NfoItemKind::Movie,
            run_time_ticks: Some(615_000_000),
            video_3d_format: Some(ferrofin_model::entities::Video3DFormat::HalfSideBySide),
            ..Default::default()
        };
        let stream = MediaStreamInfoEntity {
            stream_type: 1,
            codec: Some("mpeg4".into()),
            codec_tag: Some("XVID".into()),
            average_frame_rate: Some(1000.0),
            real_frame_rate: Some(24.0),
            is_original: true,
            is_default: true,
            is_interlaced: Some(true),
            language: Some("e\u{1}ng".into()),
            width: Some(1920),
            height: Some(1080),
            aspect_ratio: Some("16:9".into()),
            ..Default::default()
        };
        let mut writer = NfoWriter {
            buf: String::new(),
            depth: 0,
        };
        add_streams(&mut writer, &item, &[stream]);
        let xml = writer.finish();
        for tag in [
            "<codec>xvid</codec>",
            "<framerate>24</framerate>",
            "<language>eng</language>",
            "<duration>1</duration>",
            "<durationinseconds>61</durationinseconds>",
            "<original>True</original>",
            "<default>True</default>",
            "<forced>False</forced>",
            "<format3d>HSBS</format3d>",
            "<aspectratio>16:9</aspectratio>",
        ] {
            assert!(xml.contains(tag), "{tag}: {xml}");
        }
    }

    #[test]
    fn actor_thumbnails_are_written_when_supplied_by_the_runtime() {
        let result = MetadataResult {
            item: NfoBaseItem {
                kind: NfoItemKind::Movie,
                ..Default::default()
            },
            people: Some(vec![crate::container_types::PersonInfo {
                name: "Actor".into(),
                image_url: Some("/art/actor.jpg".into()),
                sort_order: Some(3),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let xml =
            super::super::save_movie(&result, &crate::xbmc::config::NfoConfiguration::default());
        assert!(xml.contains("<thumb>/art/actor.jpg</thumb>"));
        assert!(xml.contains("<sortorder>3</sortorder>"));
    }

    #[test]
    fn selected_user_data_and_sorted_artwork_are_serialized() {
        let item = NfoBaseItem {
            run_time_ticks: Some(615_000_000),
            ..Default::default()
        };
        let user:UserItemDataDto=serde_json::from_value(serde_json::json!({"Rating":8.5,"PlaybackPositionTicks":12_500_000,"PlayCount":2,"IsFavorite":true,"Played":false,"Key":"fixture","ItemId":uuid::Uuid::nil(),"LastPlayedDate":"2026-01-02T03:04:05Z"})).unwrap();
        let images: Vec<_> = [
            (ImageType::Backdrop, "/z.jpg"),
            (ImageType::Primary, "/poster.jpg"),
            (ImageType::Backdrop, "/a.jpg"),
        ]
        .into_iter()
        .map(|(image_type, path)| ItemImageInfo {
            image_type,
            path: path.into(),
            ..Default::default()
        })
        .collect();
        let mut writer = NfoWriter {
            buf: String::new(),
            depth: 0,
        };
        add_user(&mut writer, &item, &user);
        add_images(&mut writer, &images);
        let xml = writer.finish();
        for tag in [
            "<position>1.25</position>",
            "<total>61.5</total>",
            "<isuserfavorite>true</isuserfavorite>",
            "<userrating>8.5</userrating>",
            "<playcount>2</playcount>",
            "<watched>false</watched>",
            "<lastplayed>2026-01-02 03:04:05</lastplayed>",
            "<poster>/poster.jpg</poster>",
        ] {
            assert!(xml.contains(tag), "{tag}: {xml}");
        }
        assert!(xml.find("/a.jpg").unwrap() < xml.find("/z.jpg").unwrap());
        assert_eq!(seconds(-1), "-0.0000001");
        assert_eq!(seconds(10_000_000), "1");
    }
}
