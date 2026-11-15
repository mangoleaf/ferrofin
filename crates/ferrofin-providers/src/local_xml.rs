//! `collection.xml` / `playlist.xml` — port of
//! `MediaBrowser.LocalMetadata`'s `BaseItemXmlParser` (with the
//! `BoxSetXmlParser`/`PlaylistXmlParser` item lists) and of
//! `BaseXmlSaver.AddCommonNodesAsync` (`BoxSetXmlSaver`/`PlaylistXmlSaver`).
//!
//! A Jellyfin-authored collection or playlist folder carries its metadata in
//! an XML file next to the media, in a dialect distinct from the Kodi NFO one
//! ([`crate::xbmc`]): an `<Item>` root with `LocalTitle`/`Overview`/`Genres`/…
//! and a `<CollectionItems>`/`<PlaylistItems>` list of `<Path>`/`<ItemId>`
//! links. This module is the pure half: the document to and from a
//! [`LocalContainerXml`]. Which file is read when, how its result merges and
//! when one is written is the caller's.
//!
//! # Where the server reads and writes it
//!
//! - A box set's `collection.xml`: [`crate::box_set_xml`] reads it in the box
//!   set's refresh (`BoxSetXmlProvider`) and writes it from the metadata save
//!   (`BoxSetXmlSaver`).
//! - A playlist FOLDER's `playlist.xml`: the library scan reads its members
//!   and media type (`PlaylistXmlProvider`; see `ferrofin-core`'s
//!   `library_scan/playlists.rs`, whose TODO records what of it is not yet
//!   applied). Nothing writes it yet: upstream's `PlaylistXmlSaver` (its
//!   `OwnerUserId` and `<Shares>` included) is an open work item, gl-341.
//!   [`save_playlist_xml`] writes the common nodes, the item list and the
//!   media type, and is wired nowhere.
//!
//! # Upstream round-trip bugs not ported (owner decision 2026-11-14)
//!
//! `BaseXmlSaver` writes `<Added>` and `<PremiereDate>` in the server's local
//! time (`ToLocalTime()`), and `BaseItemXmlParser` reads them back as UTC
//! (`AssumeUniversal`), so on a server outside UTC every write and read
//! shifts the date added by the UTC offset and the premiere date by a day.
//! Here both are written in UTC and read as UTC, as upstream reads them: on a
//! UTC server (the container default) both write the same dates, and anywhere
//! else the file round-trips. Upstream also writes the tagline as
//! `<Taglines>` but reads only `<TagLines>`; both are read here.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use ferrofin_db::entities::base_items::PeopleEntity;
use ferrofin_model::data::{BaseItemKind, PersonKind};
use ferrofin_model::entities::MetadataField;
use ferrofin_util::finite::finite_or_none;
use serde_json::{Map, Value};

use crate::container_types::{PersonInfo, add_person};
use crate::metadata_merge::MetadataResult;
use crate::xbmc::saver::NfoWriter;
use crate::xbmc::xml_ext::{
    get_person_array, get_person_from_xml_node, get_string_array, left_part,
    read_normalized_string, try_read_date_time_exact, try_read_int,
};
use crate::xbmc::xml_reader::XmlCursor;

/// .NET ticks per minute (`TimeSpan.FromMinutes`).
const TICKS_PER_MINUTE: i64 = 600_000_000;

/// `BoxSet`'s constructor default for `DisplayOrder`, which stands until the
/// box set stores one of its own.
const BOX_SET_DISPLAY_ORDER: &str = "PremiereDate";

/// One entry of a `<CollectionItems>`/`<PlaylistItems>` list — the C#
/// `LinkedChild`, restricted to the two fields these files carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalLinkedChild {
    /// The child's on-disk path, Jellyfin's primary link key.
    pub path: Option<String>,
    /// The child's item id, when the file links by id (playlists).
    pub item_id: Option<String>,
}

impl LocalLinkedChild {
    /// Whether the entry links to anything at all — C# drops a `LinkedChild`
    /// with neither a path nor an id.
    fn is_linked(&self) -> bool {
        self.path.is_some() || self.item_id.is_some()
    }
}

/// A parsed `collection.xml` / `playlist.xml`, or the one to write.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalContainerXml {
    /// What `BaseItemXmlParser` reads into the item: its columns (the
    /// `DisplayOrder` and `RemoteTrailers` in `Data`), its people (`Some`,
    /// as `ResetPeople` leaves them, even when the file names nobody), its
    /// provider ids and its `LockedFields`. A [`MetadataResult`], as the
    /// refresh merges it.
    pub item: MetadataResult,
    /// `<PlaylistMediaType>` — a playlist's media type.
    pub playlist_media_type: Option<String>,
    /// The linked children, in file order. On a save, every manual link of
    /// the container, with the path it resolves to (`None` when it resolves
    /// to none, which upstream counts but does not write).
    pub children: Vec<LocalLinkedChild>,
}

/// Parses a `collection.xml` (`BoxSetXmlParser`, `kind` [`BaseItemKind::BoxSet`])
/// or `playlist.xml` (`PlaylistXmlParser`, [`BaseItemKind::Playlist`])
/// document — `BaseItemXmlParser.Fetch`.
///
/// `provider_keys` are the keys of the external ids the item supports
/// (`ProviderManager.GetExternalIdInfos(item)`): an element named a key plus
/// `Id` (`TmdbId`), in any case, is that provider id, and `IMDB` is always
/// `Imdb`'s. `None` for a document that does not parse (upstream's
/// `XmlException`, which leaves the item as it was).
#[must_use]
pub fn parse_container_xml(
    xml: &str,
    kind: BaseItemKind,
    provider_keys: &[String],
) -> Option<LocalContainerXml> {
    let mut cursor = XmlCursor::new(xml).ok()?;
    let mut parser = Parser {
        kind,
        provider_keys,
        out: LocalContainerXml::default(),
        people: Vec::new(),
        genres: Vec::new(),
        studios: Vec::new(),
        trailers: Vec::new(),
        data: Map::new(),
    };
    cursor.move_to_content();
    cursor.read();
    while !cursor.eof() {
        if cursor.is_element() {
            parser.node(&mut cursor);
        } else {
            cursor.read();
        }
    }
    Some(parser.finish())
}

/// The state of one `BaseItemXmlParser.Fetch`: the result, and the lists the
/// C# item grows one element at a time.
struct Parser<'a> {
    kind: BaseItemKind,
    provider_keys: &'a [String],
    out: LocalContainerXml,
    people: Vec<PersonInfo>,
    genres: Vec<String>,
    studios: Vec<String>,
    trailers: Vec<String>,
    data: Map<String, Value>,
}

impl Parser<'_> {
    /// `FetchDataFromXmlNode`, the box set's and playlist's overrides first.
    // One arm per element of upstream's one switch, kept side by side.
    #[allow(clippy::too_many_lines)]
    fn node(&mut self, reader: &mut XmlCursor) {
        let item = &mut self.out.item.item;
        match reader.name() {
            "CollectionItems" if self.kind == BaseItemKind::BoxSet => {
                self.out.children = linked_children(reader, "CollectionItem");
            }
            "PlaylistItems" if self.kind == BaseItemKind::Playlist => {
                self.out.children = linked_children(reader, "PlaylistItem");
            }
            "PlaylistMediaType" if self.kind == BaseItemKind::Playlist => {
                let value = read_normalized_string(reader);
                self.out.playlist_media_type = Some(value).filter(|v| !v.is_empty());
            }
            "Added" => {
                if let Some(date) = parse_date_time(&reader.read_element_content_as_string()) {
                    item.date_created = Some(date);
                }
            }
            "OriginalTitle" => item.original_title = Some(read_normalized_string(reader)),
            "LocalTitle" => item.name = Some(read_normalized_string(reader)),
            "CriticRating" => {
                let text = reader.read_element_content_as_string();
                if let Ok(value) = text.trim().replace(',', "").parse::<f32>() {
                    item.critic_rating = finite_or_none(Some(value)).map(f64::from);
                }
            }
            "SortTitle" => item.forced_sort_name = Some(read_normalized_string(reader)),
            "Overview" | "Description" => item.overview = Some(read_normalized_string(reader)),
            "Language" => item.preferred_metadata_language = Some(read_normalized_string(reader)),
            "CountryCode" => {
                item.preferred_metadata_country_code = Some(read_normalized_string(reader));
            }
            // A person's birthplace; nothing else keeps it.
            "PlaceOfBirth" | "AspectRatio" | "OwnerUserId" | "Format3D" => {
                let _ = reader.read_element_content_as_string();
            }
            "LockedFields" => {
                let value = reader.read_element_content_as_string();
                if !value.trim().is_empty() {
                    self.out.item.locked_fields = value
                        .split('|')
                        .filter_map(|field| {
                            ferrofin_model::json::enums::try_parse::<MetadataField>(field, true)
                        })
                        .collect();
                }
            }
            // Upstream reads only `TagLines`, and writes `Taglines`.
            "TagLines" | "Taglines" => {
                for tagline in list_values(reader, "Tagline") {
                    item.tagline = Some(tagline);
                }
            }
            "ContentRating" | "MPAARating" => {
                item.official_rating = Some(read_normalized_string(reader));
            }
            "CustomRating" => item.custom_rating = Some(read_normalized_string(reader)),
            "RunningTime" => {
                let text = read_normalized_string(reader);
                if let Ok(minutes) = left_part(&text, ' ').trim().parse::<i32>() {
                    item.run_time_ticks = Some(i64::from(minutes) * TICKS_PER_MINUTE);
                }
            }
            "LockData" => {
                item.is_locked = read_normalized_string(reader).eq_ignore_ascii_case("true");
            }
            "Network" => {
                for studio in get_string_array(reader) {
                    add_distinct(&mut self.studios, studio);
                }
            }
            "Director" => self.add_people(get_person_array(reader, PersonKind::Director)),
            "Writer" => self.add_people(get_person_array(reader, PersonKind::Writer)),
            "Actors" => self.add_people(get_person_array(reader, PersonKind::Actor)),
            "GuestStars" => self.add_people(get_person_array(reader, PersonKind::GuestStar)),
            "Trailer" => {
                let trailer = read_normalized_string(reader);
                add_distinct(&mut self.trailers, trailer);
            }
            "Trailers" => {
                for trailer in list_values(reader, "Trailer") {
                    add_distinct(&mut self.trailers, trailer);
                }
            }
            "DisplayOrder" => {
                let order = read_normalized_string(reader);
                // `IHasDisplayOrder`: of the kinds read here, a box set.
                if !order.is_empty() && self.kind == BaseItemKind::BoxSet {
                    self.data
                        .insert("DisplayOrder".to_owned(), Value::String(order));
                }
            }
            "ProductionYear" => {
                // Upstream's floor (`BaseItemXmlParser.cs:325`).
                if let Some(year) = try_read_int(reader).filter(|year| *year > 1850) {
                    item.production_year = Some(i64::from(year));
                }
            }
            "Rating" | "IMDBrating" => {
                if let Some(rating) = parse_rating(&read_normalized_string(reader)) {
                    item.community_rating = Some(f64::from(rating));
                }
            }
            "BirthDate" | "PremiereDate" | "FirstAired" => {
                if let Some(date) = try_read_date_time_exact(reader, "yyyy-MM-dd") {
                    item.premiere_date = Some(date);
                    item.production_year = Some(i64::from(chrono::Datelike::year(&date)));
                }
            }
            "DeathDate" | "EndDate" => {
                if let Some(date) = try_read_date_time_exact(reader, "yyyy-MM-dd") {
                    item.end_date = Some(date);
                }
            }
            "CollectionNumber" => {
                let id = read_normalized_string(reader);
                set_provider_id(&mut self.out.item.provider_ids, "TmdbCollection", &id);
            }
            "Genres" => {
                for genre in list_values(reader, "Genre") {
                    add_distinct(&mut self.genres, genre);
                }
            }
            "Tags" => {
                // `Tags = tags.Distinct(StringComparer.Ordinal)`: the last
                // `<Tags>` replaces any earlier one.
                let mut tags: Vec<String> = Vec::new();
                for tag in list_values(reader, "Tag") {
                    if !tags.contains(&tag) {
                        tags.push(tag);
                    }
                }
                item.tags = join(&tags);
            }
            "Persons" => {
                if reader.is_empty_element() {
                    reader.read();
                } else {
                    let mut sub = reader.read_subtree();
                    sub.move_to_content();
                    sub.read();
                    while !sub.eof() {
                        if !sub.is_element() {
                            sub.read();
                        } else if matches!(sub.name(), "Person" | "Actor") {
                            if let Some(person) = get_person_from_xml_node(&mut sub) {
                                add_person(&mut self.people, person);
                            }
                        } else {
                            sub.skip();
                        }
                    }
                }
            }
            "Studios" => {
                for studio in list_values(reader, "Studio") {
                    add_distinct(&mut self.studios, studio);
                }
            }
            // A playlist's `<Shares>` is gl-341's; a box set has none.
            "Shares" | "Countries" => reader.skip(),
            name => {
                let key = if name.eq_ignore_ascii_case("IMDB") {
                    Some("Imdb")
                } else {
                    self.provider_keys
                        .iter()
                        .find(|key| {
                            name.len() == key.len() + 2
                                && name[..key.len()].eq_ignore_ascii_case(key)
                                && name[key.len()..].eq_ignore_ascii_case("Id")
                        })
                        .map(String::as_str)
                };
                match key {
                    Some(key) => {
                        let id = read_normalized_string(reader);
                        set_provider_id(&mut self.out.item.provider_ids, key, &id);
                    }
                    None => reader.skip(),
                }
            }
        }
    }

    /// `itemResult.AddPerson` for each.
    fn add_people(&mut self, people: Vec<PersonInfo>) {
        for person in people {
            add_person(&mut self.people, person);
        }
    }

    /// The result, with the grown lists written into the row.
    fn finish(mut self) -> LocalContainerXml {
        let item = &mut self.out.item.item;
        if !self.genres.is_empty() {
            item.genres = join(&self.genres);
        }
        if !self.studios.is_empty() {
            item.studios = join(&self.studios);
        }
        if !self.trailers.is_empty() {
            let trailers = self
                .trailers
                .into_iter()
                .map(|url| {
                    let mut entry = Map::new();
                    entry.insert("Url".to_owned(), Value::String(url));
                    Value::Object(entry)
                })
                .collect();
            self.data
                .insert("RemoteTrailers".to_owned(), Value::Array(trailers));
        }
        if !self.data.is_empty() {
            item.data = serde_json::to_string(&Value::Object(self.data)).ok();
        }
        self.out.item.people = Some(self.people.into_iter().map(person_entity).collect());
        self.out
    }
}

/// `FetchFromCollectionItemsNode`: the `<item_element>` links of the list the
/// cursor is on, which replace any read before.
fn linked_children(reader: &mut XmlCursor, item_element: &str) -> Vec<LocalLinkedChild> {
    let mut children = Vec::new();
    if reader.is_empty_element() {
        reader.read();
        return children;
    }
    let mut sub = reader.read_subtree();
    sub.move_to_content();
    sub.read();
    while !sub.eof() {
        if !sub.is_element() {
            sub.read();
        } else if sub.name() == item_element && !sub.is_empty_element() {
            // `read_subtree` already advances `sub` past the entry.
            let mut entry = sub.read_subtree();
            if let Some(child) = read_linked_child(&mut entry) {
                children.push(child);
            }
        } else if sub.name() == item_element {
            sub.read();
        } else {
            sub.skip();
        }
    }
    children
}

/// Reads one `<CollectionItem>`/`<PlaylistItem>` subtree — port of
/// `BaseItemXmlParser.GetLinkedChild`.
fn read_linked_child(sub: &mut XmlCursor) -> Option<LocalLinkedChild> {
    let mut child = LocalLinkedChild::default();
    sub.move_to_content();
    sub.read();
    while !sub.eof() {
        if !sub.is_element() {
            sub.read();
            continue;
        }
        match sub.name() {
            "Path" => {
                child.path = Some(read_normalized_string(sub)).filter(|v| !v.is_empty());
            }
            "ItemId" => {
                child.item_id = Some(read_normalized_string(sub)).filter(|v| !v.is_empty());
            }
            _ => sub.skip(),
        }
    }
    child.is_linked().then_some(child)
}

/// The non-empty `<item_element>` values of the list the cursor is on
/// (`FetchFromGenresNode` and its siblings), in order.
fn list_values(reader: &mut XmlCursor, item_element: &str) -> Vec<String> {
    let mut values = Vec::new();
    if reader.is_empty_element() {
        reader.read();
        return values;
    }
    let mut sub = reader.read_subtree();
    sub.move_to_content();
    sub.read();
    while !sub.eof() {
        if !sub.is_element() {
            sub.read();
        } else if sub.name() == item_element {
            let value = read_normalized_string(&mut sub);
            if !value.is_empty() {
                values.push(value);
            }
        } else {
            sub.skip();
        }
    }
    values
}

/// `AddGenre`/`AddStudio`/`AddTrailerUrl`: a non-empty value not already
/// there in any case.
fn add_distinct(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.iter().any(|v| v.eq_ignore_ascii_case(&value)) {
        values.push(value);
    }
}

/// A `|`-joined list column, `None` when empty.
fn join(values: &[String]) -> Option<String> {
    (!values.is_empty()).then(|| values.join("|"))
}

/// `TrySetProviderId`: a blank id sets nothing; a key already there in any
/// case takes the new id.
fn set_provider_id(ids: &mut Vec<(String, String)>, key: &str, id: &str) {
    if id.trim().is_empty() {
        return;
    }
    match ids.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
        Some((_, value)) => id.clone_into(value),
        None => ids.push((key.to_owned(), id.to_owned())),
    }
}

/// `<Rating>`: `float.TryParse(rating.Replace(',', '.'),
/// NumberStyles.AllowDecimalPoint)` — digits and one decimal point, nothing
/// else.
fn parse_rating(text: &str) -> Option<f32> {
    let text = text.replace(',', ".");
    let digits = text.chars().filter(char::is_ascii_digit).count();
    let points = text.chars().filter(|c| *c == '.').count();
    (digits > 0 && digits + points == text.len() && points <= 1)
        .then(|| text.parse().ok())
        .flatten()
}

/// `TryReadDateTime`: `DateTime.TryParse` in the invariant culture, a value
/// with no zone taken as UTC. Reads the invariant general form `BaseXmlSaver`
/// writes (`MM/dd/yyyy HH:mm:ss`) and the ISO forms.
fn parse_date_time(text: &str) -> Option<DateTime<Utc>> {
    const DATE_TIMES: [&str; 3] = [
        "%m/%d/%Y %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
    ];
    let text = text.trim();
    if let Ok(date) = DateTime::parse_from_rfc3339(text) {
        return Some(date.with_timezone(&Utc));
    }
    if let Some(date) = DATE_TIMES
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
    {
        return Some(date.and_utc());
    }
    ["%m/%d/%Y", "%Y-%m-%d"]
        .iter()
        .find_map(|format| NaiveDate::parse_from_str(text, format).ok())
        .map(|date| date.and_time(chrono::NaiveTime::MIN).and_utc())
}

/// A parsed person as the people table stores one: its kind by name
/// (`PersonInfo.Type.ToString()`), no remote id or image.
fn person_entity(person: PersonInfo) -> PeopleEntity {
    PeopleEntity {
        id: ferrofin_db::store::guid_to_db(uuid::Uuid::new_v4()),
        name: person.name,
        person_type: Some(person.type_.json_name().into_owned()),
        role: person.role,
        primary_image_url: None,
        provider_id: None,
        sort_order: person.sort_order.map(i64::from),
    }
}

/// Serializes a `collection.xml` (`BoxSetXmlSaver`): the common nodes, then
/// `<CollectionItems>` with the path of each of `container.children`.
#[must_use]
pub fn save_collection_xml(container: &LocalContainerXml) -> String {
    let mut writer = NfoWriter::new();
    writer.start_element("Item");
    write_common_nodes(&mut writer, &container.item, BaseItemKind::BoxSet);
    write_linked_children(
        &mut writer,
        &container.children,
        "CollectionItems",
        "CollectionItem",
    );
    writer.end_element("Item");
    writer.finish()
}

/// Serializes a `playlist.xml` (`PlaylistXmlSaver`): the common nodes, the
/// item list, then `<PlaylistMediaType>` (`WriteCustomElementsAsync` runs
/// after `AddCommonNodesAsync`). `OwnerUserId` and `<Shares>` are gl-341's.
#[must_use]
pub fn save_playlist_xml(container: &LocalContainerXml) -> String {
    let mut writer = NfoWriter::new();
    writer.start_element("Item");
    write_common_nodes(&mut writer, &container.item, BaseItemKind::Playlist);
    write_linked_children(
        &mut writer,
        &container.children,
        "PlaylistItems",
        "PlaylistItem",
    );
    if let Some(media_type) = container
        .playlist_media_type
        .as_deref()
        .filter(|v| !v.is_empty())
    {
        writer.element("PlaylistMediaType", media_type);
    }
    writer.end_element("Item");
    writer.finish()
}

/// `BaseXmlSaver.AddCommonNodesAsync`, up to the item lists, for a box set or
/// playlist (no episode, person or video arm applies).
// One block per element, in upstream's order.
#[allow(clippy::too_many_lines)]
fn write_common_nodes(writer: &mut NfoWriter, result: &MetadataResult, kind: BaseItemKind) {
    let item = &result.item;
    let data: Map<String, Value> = item
        .data
        .as_deref()
        .and_then(|data| serde_json::from_str(data).ok())
        .unwrap_or_default();
    if let Some(rating) = present(item.official_rating.as_ref()) {
        writer.element("ContentRating", rating);
    }
    // `DateTime.MinValue` when unset; UTC (see the module doc).
    let added = item.date_created.map_or_else(
        || "01/01/0001 00:00:00".to_owned(),
        |date| date.format("%m/%d/%Y %H:%M:%S").to_string(),
    );
    writer.element("Added", &added);
    writer.element("LockData", if item.is_locked { "true" } else { "false" });
    if !result.locked_fields.is_empty() {
        let fields: Vec<String> = result
            .locked_fields
            .iter()
            .map(|field| field.json_name().into_owned())
            .collect();
        writer.element("LockedFields", &fields.join("|"));
    }
    if let Some(rating) = item.critic_rating {
        writer.element("CriticRating", &float(rating));
    }
    if let Some(overview) = present(item.overview.as_ref()) {
        writer.element("Overview", overview);
    }
    if let Some(title) = present(item.original_title.as_ref()) {
        writer.element("OriginalTitle", title);
    }
    if let Some(rating) = present(item.custom_rating.as_ref()) {
        writer.element("CustomRating", rating);
    }
    if let Some(name) = present(item.name.as_ref()) {
        writer.element("LocalTitle", name);
    }
    if let Some(sort) = present(item.forced_sort_name.as_ref()) {
        writer.element("SortTitle", sort);
    }
    if let Some(date) = item.premiere_date {
        writer.element("PremiereDate", &date.format("%Y-%m-%d").to_string());
    }
    if let Some(date) = item.end_date {
        writer.element("EndDate", &date.format("%Y-%m-%d").to_string());
    }
    let trailers: Vec<&str> = data
        .get("RemoteTrailers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|trailer| trailer.get("Url").and_then(Value::as_str))
        .collect();
    write_list(writer, "Trailers", "Trailer", &trailers);
    write_list(
        writer,
        "Countries",
        "Country",
        &split(item.production_locations.as_deref()),
    );
    if kind == BaseItemKind::BoxSet {
        let order = data
            .get("DisplayOrder")
            .and_then(Value::as_str)
            .unwrap_or(BOX_SET_DISPLAY_ORDER);
        if !order.is_empty() {
            writer.element("DisplayOrder", order);
        }
    }
    if let Some(rating) = item.community_rating {
        writer.element("Rating", &float(rating));
    }
    if let Some(year) = item.production_year {
        writer.element("ProductionYear", &year.to_string());
    }
    if let Some(language) = present(item.preferred_metadata_language.as_ref()) {
        writer.element("Language", language);
    }
    if let Some(country) = present(item.preferred_metadata_country_code.as_ref()) {
        writer.element("CountryCode", country);
    }
    if let Some(ticks) = item.run_time_ticks {
        // `Math.Floor(TimeSpan.FromTicks(ticks).TotalMinutes)`.
        writer.element(
            "RunningTime",
            &ticks.div_euclid(TICKS_PER_MINUTE).to_string(),
        );
    }
    for (key, id) in &result.provider_ids {
        // An element needs a name XML takes; upstream's writer would throw.
        if !id.is_empty() && is_xml_name(key) {
            writer.element(&format!("{key}Id"), id);
        }
    }
    if let Some(tagline) = item.tagline.as_deref().filter(|v| !v.trim().is_empty()) {
        writer.start_element("Taglines");
        writer.element("Tagline", tagline);
        writer.end_element("Taglines");
    }
    write_list(writer, "Genres", "Genre", &split(item.genres.as_deref()));
    write_list(writer, "Studios", "Studio", &split(item.studios.as_deref()));
    write_list(writer, "Tags", "Tag", &split(item.tags.as_deref()));
    let people = result.people.as_deref().unwrap_or_default();
    if !people.is_empty() {
        writer.start_element("Persons");
        for person in people {
            writer.start_element("Person");
            writer.element("Name", &person.name);
            let kind = person
                .person_type
                .as_deref()
                .map_or(PersonKind::Unknown, PersonKind::from_stored);
            writer.element("Type", &kind.json_name());
            writer.element("Role", person.role.as_deref().unwrap_or_default());
            if let Some(order) = person.sort_order {
                writer.element("SortOrder", &order.to_string());
            }
            writer.end_element("Person");
        }
        writer.end_element("Persons");
    }
}

/// A text column that is set and not empty (`!string.IsNullOrEmpty`).
fn present(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|v| !v.is_empty())
}

/// `AddLinkedChildren`: nothing without a manual link, else the list with the
/// path of each link that resolved to one.
fn write_linked_children(
    writer: &mut NfoWriter,
    children: &[LocalLinkedChild],
    list_element: &str,
    item_element: &str,
) {
    if children.is_empty() {
        return;
    }
    let paths: Vec<&str> = children
        .iter()
        .filter_map(|child| child.path.as_deref())
        .filter(|path| !path.trim().is_empty())
        .collect();
    if paths.is_empty() {
        writer.element(list_element, "");
        return;
    }
    writer.start_element(list_element);
    for path in paths {
        writer.start_element(item_element);
        writer.element("Path", path);
        writer.end_element(item_element);
    }
    writer.end_element(list_element);
}

/// A `<list><item>…</item></list>` block, nothing when `values` is empty.
fn write_list(writer: &mut NfoWriter, list_element: &str, item_element: &str, values: &[&str]) {
    if values.is_empty() {
        return;
    }
    writer.start_element(list_element);
    for value in values {
        writer.element(item_element, value);
    }
    writer.end_element(list_element);
}

/// The values of a `|`-joined list column.
fn split(value: Option<&str>) -> Vec<&str> {
    value
        .unwrap_or_default()
        .split('|')
        .filter(|v| !v.is_empty())
        .collect()
}

/// A stored rating as `float.ToString(CultureInfo.InvariantCulture)` writes
/// the `float` it is upstream: the shortest text that reads back.
#[allow(clippy::cast_possible_truncation)]
fn float(value: f64) -> String {
    (value as f32).to_string()
}

/// Whether `{key}Id` is an element name `XmlWriter` accepts: a letter or `_`,
/// then letters, digits, `_`, `-` or `.`.
fn is_xml_name(key: &str) -> bool {
    key.chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && key
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn keys() -> Vec<String> {
        vec!["Tmdb".to_owned()]
    }

    fn parse_box_set(xml: &str) -> LocalContainerXml {
        parse_container_xml(xml, BaseItemKind::BoxSet, &keys()).expect("parse")
    }

    #[test]
    fn a_collection_xml_yields_its_title_and_members() {
        let parsed = parse_box_set(
            r"<Item>
                <LocalTitle>The Matrix Collection</LocalTitle>
                <Overview>Neo.</Overview>
                <CollectionItems>
                  <CollectionItem><Path>/media/The Matrix.mkv</Path></CollectionItem>
                  <CollectionItem><Path>/media/Reloaded.mkv</Path></CollectionItem>
                </CollectionItems>
              </Item>",
        );
        assert_eq!(
            parsed.item.item.name.as_deref(),
            Some("The Matrix Collection")
        );
        assert_eq!(parsed.item.item.overview.as_deref(), Some("Neo."));
        assert_eq!(parsed.children.len(), 2);
        assert_eq!(
            parsed.children[0].path.as_deref(),
            Some("/media/The Matrix.mkv")
        );
    }

    /// Every field `BaseItemXmlParser` reads into a box set, from a document
    /// in the shape `BoxSetXmlSaver` writes (the date added in the invariant
    /// general form, read as UTC).
    #[test]
    #[allow(clippy::too_many_lines, reason = "one document, every field asserted")]
    fn a_jellyfin_collection_xml_yields_every_field() {
        let parsed = parse_box_set(
            r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<Item>
  <ContentRating>PG-13</ContentRating>
  <Added>11/14/2026 13:05:09</Added>
  <LockData>true</LockData>
  <LockedFields>Name|overview|Bogus|4</LockedFields>
  <CriticRating>87</CriticRating>
  <Overview>  Neo.  </Overview>
  <OriginalTitle>Matrix</OriginalTitle>
  <CustomRating>Family</CustomRating>
  <LocalTitle>The Matrix Collection</LocalTitle>
  <SortTitle>Matrix, The</SortTitle>
  <PremiereDate>1999-03-31</PremiereDate>
  <EndDate>2003-11-05</EndDate>
  <Trailers>
    <Trailer>https://example.com/a</Trailer>
    <Trailer>https://EXAMPLE.com/A</Trailer>
  </Trailers>
  <Countries>
    <Country>United States</Country>
  </Countries>
  <DisplayOrder>SortName</DisplayOrder>
  <Rating>7,5</Rating>
  <ProductionYear>1850</ProductionYear>
  <Language>en</Language>
  <CountryCode>US</CountryCode>
  <RunningTime>136 min</RunningTime>
  <TmdbId>2344</TmdbId>
  <IMDB>tt0133093</IMDB>
  <TvdbId>ignored</TvdbId>
  <Taglines>
    <Tagline>Free your mind.</Tagline>
  </Taglines>
  <Genres>
    <Genre>Action</Genre>
    <Genre>action</Genre>
    <Genre>Science Fiction</Genre>
  </Genres>
  <Studios>
    <Studio>Warner Bros.</Studio>
  </Studios>
  <Network>Village Roadshow|warner bros.</Network>
  <Tags>
    <Tag>cyberpunk</Tag>
    <Tag>cyberpunk</Tag>
    <Tag>Cyberpunk</Tag>
  </Tags>
  <Persons>
    <Person>
      <Name>Keanu Reeves</Name>
      <Type>Actor</Type>
      <Role>Neo</Role>
      <SortOrder>0</SortOrder>
    </Person>
    <Person>
      <Name>Lana Wachowski</Name>
      <Type>Director</Type>
      <Role />
    </Person>
  </Persons>
  <CollectionItems>
    <CollectionItem>
      <Path>/media/The Matrix.mkv</Path>
    </CollectionItem>
    <CollectionItem />
  </CollectionItems>
</Item>"#,
        );
        let item = &parsed.item.item;
        assert_eq!(item.official_rating.as_deref(), Some("PG-13"));
        assert_eq!(
            item.date_created,
            Some(Utc.with_ymd_and_hms(2026, 11, 14, 13, 5, 9).unwrap())
        );
        assert!(item.is_locked);
        assert_eq!(
            parsed.item.locked_fields,
            vec![
                MetadataField::Name,
                MetadataField::Overview,
                MetadataField::Tags
            ],
            "names in any case and numbers parse, the rest is dropped"
        );
        assert_eq!(item.critic_rating, Some(87.0));
        assert_eq!(item.overview.as_deref(), Some("Neo."));
        assert_eq!(item.original_title.as_deref(), Some("Matrix"));
        assert_eq!(item.custom_rating.as_deref(), Some("Family"));
        assert_eq!(item.name.as_deref(), Some("The Matrix Collection"));
        assert_eq!(item.forced_sort_name.as_deref(), Some("Matrix, The"));
        assert_eq!(
            item.premiere_date,
            Some(Utc.with_ymd_and_hms(1999, 3, 31, 0, 0, 0).unwrap())
        );
        // The premiere date sets the year; `1850` is no year.
        assert_eq!(item.production_year, Some(1999));
        assert_eq!(
            item.end_date,
            Some(Utc.with_ymd_and_hms(2003, 11, 5, 0, 0, 0).unwrap())
        );
        assert_eq!(item.community_rating, Some(7.5));
        assert_eq!(item.preferred_metadata_language.as_deref(), Some("en"));
        assert_eq!(item.preferred_metadata_country_code.as_deref(), Some("US"));
        assert_eq!(item.run_time_ticks, Some(136 * TICKS_PER_MINUTE));
        assert_eq!(
            parsed.item.provider_ids,
            vec![
                ("Tmdb".to_owned(), "2344".to_owned()),
                ("Imdb".to_owned(), "tt0133093".to_owned()),
            ],
            "only the item's own external ids, and IMDB"
        );
        assert_eq!(item.tagline.as_deref(), Some("Free your mind."));
        assert_eq!(item.genres.as_deref(), Some("Action|Science Fiction"));
        assert_eq!(
            item.studios.as_deref(),
            Some("Warner Bros.|Village Roadshow")
        );
        assert_eq!(item.tags.as_deref(), Some("cyberpunk|Cyberpunk"));
        // `<Countries>` is skipped on read.
        assert_eq!(item.production_locations, None);
        let data: Value = serde_json::from_str(item.data.as_deref().unwrap()).unwrap();
        assert_eq!(data["DisplayOrder"], "SortName");
        assert_eq!(
            data["RemoteTrailers"],
            serde_json::json!([{ "Url": "https://example.com/a" }])
        );
        let people = parsed.item.people.as_deref().unwrap();
        let credits: Vec<_> = people
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    p.person_type.as_deref(),
                    p.role.as_deref(),
                    p.sort_order,
                )
            })
            .collect();
        assert_eq!(
            credits,
            [
                ("Keanu Reeves", Some("Actor"), Some("Neo"), Some(0)),
                ("Lana Wachowski", Some("Director"), Some(""), None),
            ]
        );
        assert_eq!(
            parsed.children,
            vec![LocalLinkedChild {
                path: Some("/media/The Matrix.mkv".into()),
                item_id: None,
            }]
        );
    }

    /// `ResetPeople`: a file with no people still answers with an empty list.
    #[test]
    fn a_file_without_people_has_an_empty_cast() {
        let parsed = parse_box_set("<Item><LocalTitle>Saga</LocalTitle></Item>");
        assert_eq!(parsed.item.people, Some(Vec::new()));
        assert_eq!(parsed.item.item.data, None);
    }

    #[test]
    fn a_compact_document_keeps_every_member() {
        // No whitespace between entries. `read_subtree` already advances the
        // parent cursor, so an extra `skip()` here silently dropped every
        // second child — invisible to any test whose XML is indented.
        let parsed = parse_box_set(
            "<Item><CollectionItems>\
             <CollectionItem><Path>/a.mkv</Path></CollectionItem>\
             <CollectionItem><Path>/b.mkv</Path></CollectionItem>\
             <CollectionItem><Path>/c.mkv</Path></CollectionItem>\
             </CollectionItems></Item>",
        );
        let paths: Vec<_> = parsed
            .children
            .iter()
            .filter_map(|c| c.path.as_deref())
            .collect();
        assert_eq!(paths, ["/a.mkv", "/b.mkv", "/c.mkv"]);
    }

    /// Each parser reads only its own list: `BoxSetXmlParser` has no
    /// `PlaylistItems` case, and a later list replaces an earlier one.
    #[test]
    fn a_box_set_reads_only_its_own_item_list() {
        let parsed = parse_box_set(
            "<Item>\
             <PlaylistItems><PlaylistItem><Path>/p.mkv</Path></PlaylistItem></PlaylistItems>\
             <CollectionItems><CollectionItem><Path>/a.mkv</Path></CollectionItem></CollectionItems>\
             <CollectionItems><CollectionItem><Path>/b.mkv</Path></CollectionItem></CollectionItems>\
             <PlaylistMediaType>Audio</PlaylistMediaType>\
             </Item>",
        );
        assert_eq!(
            parsed.children,
            vec![LocalLinkedChild {
                path: Some("/b.mkv".into()),
                item_id: None,
            }]
        );
        assert_eq!(parsed.playlist_media_type, None);
    }

    #[test]
    fn a_playlist_xml_yields_its_media_type_and_id_links() {
        let parsed = parse_container_xml(
            r"<Item>
                <LocalTitle>Road Trip</LocalTitle>
                <PlaylistMediaType>Audio</PlaylistMediaType>
                <DisplayOrder>SortName</DisplayOrder>
                <PlaylistItems>
                  <PlaylistItem><ItemId>abc123</ItemId><Path>/music/a.flac</Path></PlaylistItem>
                </PlaylistItems>
              </Item>",
            BaseItemKind::Playlist,
            &[],
        )
        .expect("parse");
        assert_eq!(parsed.playlist_media_type.as_deref(), Some("Audio"));
        assert_eq!(parsed.children[0].item_id.as_deref(), Some("abc123"));
        assert_eq!(parsed.children[0].path.as_deref(), Some("/music/a.flac"));
        // A playlist has no `DisplayOrder` (`IHasDisplayOrder`).
        assert_eq!(parsed.item.item.data, None);
    }

    #[test]
    fn an_item_linking_to_nothing_is_dropped() {
        let parsed = parse_box_set(
            r"<Item><CollectionItems><CollectionItem><Type>Manual</Type></CollectionItem>
              <CollectionItem><Path>  </Path></CollectionItem>
              </CollectionItems></Item>",
        );
        assert!(parsed.children.is_empty(), "{:?}", parsed.children);
    }

    #[test]
    fn a_malformed_document_is_no_metadata() {
        assert_eq!(
            parse_container_xml(
                "<Item><LocalTitle>x</LocalTitle><Overview",
                BaseItemKind::BoxSet,
                &[]
            ),
            None
        );
    }

    #[rstest::rstest]
    #[case::invariant_general("11/14/2026 13:05:09", Some((2026, 11, 14, 13, 5, 9)))]
    #[case::single_digits("1/2/2026 3:04:05", Some((2026, 1, 2, 3, 4, 5)))]
    #[case::iso_utc("2026-11-14T13:05:09Z", Some((2026, 11, 14, 13, 5, 9)))]
    #[case::iso_offset("2026-11-14T13:05:09+02:00", Some((2026, 11, 14, 11, 5, 9)))]
    #[case::iso_no_zone("2026-11-14 13:05:09", Some((2026, 11, 14, 13, 5, 9)))]
    #[case::date_only("2026-11-14", Some((2026, 11, 14, 0, 0, 0)))]
    #[case::garbage("yesterday", None)]
    fn the_date_added_reads_as_utc(
        #[case] text: &str,
        #[case] expected: Option<(i32, u32, u32, u32, u32, u32)>,
    ) {
        assert_eq!(
            parse_date_time(text),
            expected.map(|(y, mo, d, h, mi, s)| Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap())
        );
    }

    #[rstest::rstest]
    #[case("7.5", Some(7.5))]
    #[case("7,5", Some(7.5))]
    #[case("8", Some(8.0))]
    #[case("-1", None)]
    #[case("1e3", None)]
    #[case("1.2.3", None)]
    #[case(".", None)]
    fn a_rating_takes_only_digits_and_a_decimal_point(
        #[case] text: &str,
        #[case] expected: Option<f32>,
    ) {
        assert_eq!(parse_rating(text), expected);
    }

    fn full_box_set() -> LocalContainerXml {
        LocalContainerXml {
            item: MetadataResult {
                item: ferrofin_db::entities::base_items::BaseItemEntity {
                    name: Some("Bond & Co".into()),
                    overview: Some("Shaken <not> stirred".into()),
                    official_rating: Some("PG".into()),
                    date_created: Some(Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()),
                    is_locked: true,
                    critic_rating: Some(f64::from(7.1_f32)),
                    community_rating: Some(6.5),
                    original_title: Some("Bond".into()),
                    custom_rating: Some("Teen".into()),
                    forced_sort_name: Some("Bond".into()),
                    premiere_date: Some(Utc.with_ymd_and_hms(1962, 10, 5, 0, 0, 0).unwrap()),
                    end_date: Some(Utc.with_ymd_and_hms(2021, 9, 30, 0, 0, 0).unwrap()),
                    production_year: Some(1962),
                    preferred_metadata_language: Some("en".into()),
                    preferred_metadata_country_code: Some("GB".into()),
                    run_time_ticks: Some(95 * TICKS_PER_MINUTE + 1),
                    tagline: Some("License to kill".into()),
                    genres: Some("Action|Thriller".into()),
                    studios: Some("Eon".into()),
                    tags: Some("spy".into()),
                    data: Some(
                        r#"{"DisplayOrder":"SortName","RemoteTrailers":[{"Url":"https://t/1"}]}"#
                            .into(),
                    ),
                    ..Default::default()
                },
                people: Some(vec![PeopleEntity {
                    name: "Sean Connery".into(),
                    person_type: Some("Actor".into()),
                    role: Some("James Bond".into()),
                    sort_order: Some(1),
                    ..Default::default()
                }]),
                provider_ids: vec![("Tmdb".into(), "645".into())],
                locked_fields: vec![MetadataField::Name, MetadataField::Overview],
            },
            playlist_media_type: None,
            children: vec![
                LocalLinkedChild {
                    path: Some("/media/Dr. No.mkv".into()),
                    item_id: None,
                },
                LocalLinkedChild::default(),
            ],
        }
    }

    /// `BoxSetXmlSaver`'s document: the common nodes in upstream's order, two-space
    /// indent, empty values as empty elements, an unresolved link left out.
    #[test]
    fn a_collection_saves_in_upstreams_shape() {
        let xml = save_collection_xml(&full_box_set());
        assert_eq!(
            xml,
            r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<Item>
  <ContentRating>PG</ContentRating>
  <Added>01/02/2026 03:04:05</Added>
  <LockData>true</LockData>
  <LockedFields>Name|Overview</LockedFields>
  <CriticRating>7.1</CriticRating>
  <Overview>Shaken &lt;not&gt; stirred</Overview>
  <OriginalTitle>Bond</OriginalTitle>
  <CustomRating>Teen</CustomRating>
  <LocalTitle>Bond &amp; Co</LocalTitle>
  <SortTitle>Bond</SortTitle>
  <PremiereDate>1962-10-05</PremiereDate>
  <EndDate>2021-09-30</EndDate>
  <Trailers>
    <Trailer>https://t/1</Trailer>
  </Trailers>
  <DisplayOrder>SortName</DisplayOrder>
  <Rating>6.5</Rating>
  <ProductionYear>1962</ProductionYear>
  <Language>en</Language>
  <CountryCode>GB</CountryCode>
  <RunningTime>95</RunningTime>
  <TmdbId>645</TmdbId>
  <Taglines>
    <Tagline>License to kill</Tagline>
  </Taglines>
  <Genres>
    <Genre>Action</Genre>
    <Genre>Thriller</Genre>
  </Genres>
  <Studios>
    <Studio>Eon</Studio>
  </Studios>
  <Tags>
    <Tag>spy</Tag>
  </Tags>
  <Persons>
    <Person>
      <Name>Sean Connery</Name>
      <Type>Actor</Type>
      <Role>James Bond</Role>
      <SortOrder>1</SortOrder>
    </Person>
  </Persons>
  <CollectionItems>
    <CollectionItem>
      <Path>/media/Dr. No.mkv</Path>
    </CollectionItem>
  </CollectionItems>
</Item>
"#
        );
    }

    /// What the saver writes, the parser reads back: the file round-trips.
    #[test]
    fn a_collection_survives_a_save_and_reparse() {
        let original = full_box_set();
        let xml = save_collection_xml(&original);
        let reparsed = parse_container_xml(&xml, BaseItemKind::BoxSet, &keys()).expect("reparse");
        let (was, now) = (&original.item.item, &reparsed.item.item);
        assert_eq!(now.name, was.name);
        assert_eq!(now.overview, was.overview);
        assert_eq!(now.official_rating, was.official_rating);
        assert_eq!(now.date_created, was.date_created);
        assert_eq!(now.is_locked, was.is_locked);
        assert_eq!(now.critic_rating, was.critic_rating);
        assert_eq!(now.community_rating, was.community_rating);
        assert_eq!(now.original_title, was.original_title);
        assert_eq!(now.custom_rating, was.custom_rating);
        assert_eq!(now.forced_sort_name, was.forced_sort_name);
        assert_eq!(now.premiere_date, was.premiere_date);
        assert_eq!(now.end_date, was.end_date);
        assert_eq!(now.production_year, was.production_year);
        assert_eq!(
            now.preferred_metadata_language,
            was.preferred_metadata_language
        );
        assert_eq!(
            now.preferred_metadata_country_code,
            was.preferred_metadata_country_code
        );
        assert_eq!(now.run_time_ticks, Some(95 * TICKS_PER_MINUTE));
        assert_eq!(now.tagline, was.tagline);
        assert_eq!(now.genres, was.genres);
        assert_eq!(now.studios, was.studios);
        assert_eq!(now.tags, was.tags);
        let data: Value = serde_json::from_str(now.data.as_deref().unwrap()).unwrap();
        assert_eq!(
            data,
            serde_json::json!({"DisplayOrder":"SortName","RemoteTrailers":[{"Url":"https://t/1"}]})
        );
        assert_eq!(reparsed.item.provider_ids, original.item.provider_ids);
        assert_eq!(reparsed.item.locked_fields, original.item.locked_fields);
        let person = &reparsed.item.people.as_deref().unwrap()[0];
        assert_eq!(
            (
                person.name.as_str(),
                person.role.as_deref(),
                person.sort_order
            ),
            ("Sean Connery", Some("James Bond"), Some(1))
        );
        assert_eq!(reparsed.children, original.children[..1]);
    }

    /// A box set with no `DisplayOrder` of its own writes `BoxSet`'s default,
    /// and links that all failed to resolve still write the (empty) list.
    #[test]
    fn a_bare_box_set_writes_its_default_order_and_an_empty_list() {
        let xml = save_collection_xml(&LocalContainerXml {
            item: MetadataResult::of(ferrofin_db::entities::base_items::BaseItemEntity {
                name: Some("Empty".into()),
                ..Default::default()
            }),
            children: vec![LocalLinkedChild::default()],
            ..LocalContainerXml::default()
        });
        assert!(xml.contains("<Added>01/01/0001 00:00:00</Added>"), "{xml}");
        assert!(xml.contains("<LockData>false</LockData>"), "{xml}");
        assert!(
            xml.contains("<DisplayOrder>PremiereDate</DisplayOrder>"),
            "{xml}"
        );
        assert!(xml.contains("<CollectionItems />"), "{xml}");
        assert!(!xml.contains("<Persons>"), "{xml}");
    }

    #[test]
    fn an_empty_container_writes_no_item_list() {
        let xml = save_collection_xml(&LocalContainerXml {
            item: MetadataResult::of(ferrofin_db::entities::base_items::BaseItemEntity {
                name: Some("Empty".into()),
                ..Default::default()
            }),
            ..LocalContainerXml::default()
        });
        assert!(!xml.contains("<CollectionItems"));
        assert!(xml.contains("<LocalTitle>Empty</LocalTitle>"));
    }

    #[test]
    fn a_playlist_survives_a_save_and_reparse() {
        let original = LocalContainerXml {
            item: MetadataResult::of(ferrofin_db::entities::base_items::BaseItemEntity {
                name: Some("Road Trip".into()),
                ..Default::default()
            }),
            playlist_media_type: Some("Audio".into()),
            children: vec![
                LocalLinkedChild {
                    path: Some("/music/a.flac".into()),
                    item_id: Some("abc123".into()),
                },
                LocalLinkedChild {
                    path: Some("/music/b.flac".into()),
                    item_id: None,
                },
            ],
        };
        let xml = save_playlist_xml(&original);
        assert!(xml.contains("<PlaylistMediaType>Audio</PlaylistMediaType>"));
        assert!(!xml.contains("<DisplayOrder>"), "{xml}");
        // C# writes the media type from `WriteCustomElementsAsync`, which runs
        // after the common nodes have emitted the item list.
        assert!(
            xml.find("<PlaylistItems>") < xml.find("<PlaylistMediaType>"),
            "the item list comes first: {xml}"
        );
        // Only `<Path>` is written (C# `AddLinkedChildren` resolves ids to
        // paths), so an id does not survive the round trip — the reader still
        // accepts one, because a Jellyfin-written file may carry it.
        let reparsed = parse_container_xml(&xml, BaseItemKind::Playlist, &[]).expect("reparse");
        assert_eq!(
            reparsed.children,
            vec![
                LocalLinkedChild {
                    path: Some("/music/a.flac".into()),
                    item_id: None,
                },
                LocalLinkedChild {
                    path: Some("/music/b.flac".into()),
                    item_id: None,
                },
            ]
        );
        assert_eq!(reparsed.item.item.name.as_deref(), Some("Road Trip"));
        assert_eq!(reparsed.playlist_media_type, original.playlist_media_type);
    }

    /// A provider key `XmlWriter` cannot name an element after is left out.
    #[test]
    fn a_provider_key_that_is_no_xml_name_is_not_written() {
        let mut container = full_box_set();
        container.item.provider_ids = vec![
            ("My Plugin".into(), "1".into()),
            ("Tmdb".into(), String::new()),
            ("TmdbCollection".into(), "10".into()),
        ];
        let xml = save_collection_xml(&container);
        assert!(!xml.contains("My Plugin"), "{xml}");
        assert!(!xml.contains("<TmdbId>"), "{xml}");
        assert!(
            xml.contains("<TmdbCollectionId>10</TmdbCollectionId>"),
            "{xml}"
        );
    }
}
