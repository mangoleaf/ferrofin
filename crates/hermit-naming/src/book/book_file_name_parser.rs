//! Port of `Emby.Naming.Book.BookFileNameParser`.

use std::sync::OnceLock;

use regex::Regex;

use crate::book::BookFileNameParserResult;

fn name_matches() -> &'static [Regex] {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        [
            // seriesName (seriesYear) #index (of count) (year), where only
            // seriesName and index are required.
            r"^(?<seriesName>.+?)((\s\((?<seriesYear>[0-9]{4})\))?)\s#(?<index>[0-9]+)(?:\.0)?((\s\(of\s(?<count>[0-9]+)\))?)((\s\((?<year>[0-9]{4})\))?)$",
            r"^(?<name>.+?)\s\((?<seriesName>.+?),\s#(?<index>[0-9]+)\)(?:\.0)?((\s\((?<year>[0-9]{4})\))?)$",
            r"^(?<index>[0-9]+)(?:\.0)?\s\-\s(?<name>.+?)((\s\((?<year>[0-9]{4})\))?)$",
            r"(?<name>.*)\((?<year>[0-9]{4})\)",
            // Last resort: match the whole string as the name.
            r"(?<name>.*)",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("book name regex valid"))
        .collect()
    })
}

fn comic_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?<name>.+?)(\sv(?<volume>[0-9]+))?(\sc(?<chapter>[0-9]+))?$")
            .expect("comic regex valid")
    })
}

/// Parses a filename to retrieve the book name, series name, index, and year.
#[must_use]
pub fn parse(name: Option<&str>) -> BookFileNameParserResult {
    let mut result = BookFileNameParserResult::default();

    let Some(name) = name else {
        return result;
    };

    for regex in name_matches() {
        let Some(captures) = regex.captures(name) else {
            continue;
        };

        if let Some(name_group) = captures.name("name") {
            if let Some(comic) = comic_regex().captures(name_group.as_str().trim()) {
                if let Some(v) = parse_group(&comic, "volume") {
                    result.parent_index = Some(v);
                }
                if let Some(c) = parse_group(&comic, "chapter") {
                    result.index = Some(c);
                }
            }

            result.name = Some(name_group.as_str().trim().to_string());
        }

        if let Some(index) = parse_group(&captures, "index") {
            result.index = Some(index);
        }

        if let Some(year) = parse_group(&captures, "year") {
            result.year = Some(year);
        }

        if let Some(series_group) = captures.name("seriesName") {
            result.series_name = Some(series_group.as_str().trim().to_string());
        }

        break;
    }

    result
}

fn parse_group(captures: &regex::Captures<'_>, name: &str) -> Option<i32> {
    captures.name(name)?.as_str().parse::<i32>().ok()
}
