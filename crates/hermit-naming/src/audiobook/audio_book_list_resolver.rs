//! Port of `Emby.Naming.AudioBook.AudioBookListResolver`.

use crate::audiobook::{
    AudioBookFileInfo, AudioBookInfo, AudioBookNameParser, AudioBookNameParserResult,
    AudioBookResolver,
};
use crate::common::NamingOptions;
use crate::io::FileSystemMetadata;
use crate::path;
use crate::video::stack_resolver;

/// Resolves name, year, alternate files and extras from a stack of audiobook
/// files.
pub struct AudioBookListResolver<'a> {
    options: &'a NamingOptions,
}

impl<'a> AudioBookListResolver<'a> {
    /// Creates a new [`AudioBookListResolver`].
    #[must_use]
    pub fn new(options: &'a NamingOptions) -> Self {
        Self { options }
    }

    /// Resolves name, year, alternate files and extras from `files`.
    #[must_use]
    pub fn resolve(&self, files: &[FileSystemMetadata]) -> Vec<AudioBookInfo> {
        let resolver = AudioBookResolver::new(self.options);

        // Files with empty full-name are dropped here.
        let audiobook_file_infos: Vec<AudioBookFileInfo> = files
            .iter()
            .filter_map(|i| resolver.resolve(&i.full_name))
            .collect();

        let stack_result = stack_resolver::resolve_audio_books(&audiobook_file_infos);

        let mut result = Vec::new();
        for stack in stack_result {
            let mut stack_files: Vec<AudioBookFileInfo> = stack
                .files
                .iter()
                .filter_map(|i| resolver.resolve(i))
                .collect();

            stack_files.sort();

            let name_parser_result = AudioBookNameParser::new(self.options).parse(&stack.name);

            let (extras, alternate_versions) =
                find_extra_and_alternative_files(&mut stack_files, &name_parser_result);

            result.push(AudioBookInfo::new(
                name_parser_result.name.clone().unwrap_or_default(),
                name_parser_result.year,
                stack_files,
                extras,
                alternate_versions,
            ));
        }

        result
    }
}

fn find_extra_and_alternative_files(
    stack_files: &mut Vec<AudioBookFileInfo>,
    name_parser_result: &AudioBookNameParserResult,
) -> (Vec<AudioBookFileInfo>, Vec<AudioBookFileInfo>) {
    let mut extras: Vec<AudioBookFileInfo> = Vec::new();
    let mut alternative_versions: Vec<AudioBookFileInfo> = Vec::new();

    let name = name_parser_result.name.clone().unwrap_or_default();
    let have_chapters_or_pages = stack_files
        .iter()
        .any(|x| x.chapter_number.is_some() || x.part_number.is_some());
    let name_with_replaced_dots = name.replace(' ', ".");

    // Group by (chapter, part), preserving encounter order.
    let mut keys: Vec<(Option<i32>, Option<i32>)> = Vec::new();
    let mut groups: Vec<Vec<AudioBookFileInfo>> = Vec::new();
    for file in stack_files.iter() {
        let key = (file.chapter_number, file.part_number);
        if let Some(pos) = keys.iter().position(|k| *k == key) {
            groups[pos].push(file.clone());
        } else {
            keys.push(key);
            groups.push(vec![file.clone()]);
        }
    }

    for (key, group) in keys.iter().zip(groups.iter()) {
        if key.0.is_none() && key.1.is_none() {
            if group.len() > 1 || have_chapters_or_pages {
                let mut ex: Vec<AudioBookFileInfo> = Vec::new();
                let mut alt: Vec<AudioBookFileInfo> = Vec::new();

                for audio_file in group {
                    let file_name = path::file_name_without_extension(&audio_file.path);
                    if file_name.eq_ignore_ascii_case("audiobook")
                        || contains_ignore_ascii_case(file_name, &name)
                        || contains_ignore_ascii_case(file_name, &name_with_replaced_dots)
                    {
                        alt.push(audio_file.clone());
                    } else {
                        ex.push(audio_file.clone());
                    }
                }

                if !ex.is_empty() {
                    ex.sort_by(|a, b| {
                        a.container
                            .cmp(&b.container)
                            .then_with(|| a.path.cmp(&b.path))
                    });
                    remove_all(stack_files, &ex);
                    extras.extend(ex);
                }

                if !alt.is_empty() {
                    alt.sort_by(|a, b| {
                        a.container
                            .cmp(&b.container)
                            .then_with(|| a.path.cmp(&b.path))
                    });
                    let main = find_main_audio_book_file(&alt, &name);
                    let alternatives: Vec<AudioBookFileInfo> =
                        alt.into_iter().filter(|f| f != &main).collect();
                    remove_all(stack_files, &alternatives);
                    alternative_versions.extend(alternatives);
                }
            }
        } else if group.len() > 1 {
            let mut sorted = group.clone();
            sorted.sort_by(|a, b| {
                a.container
                    .cmp(&b.container)
                    .then_with(|| a.path.cmp(&b.path))
            });
            let alternatives: Vec<AudioBookFileInfo> = sorted.into_iter().skip(1).collect();
            remove_all(stack_files, &alternatives);
            alternative_versions.extend(alternatives);
        }
    }

    (extras, alternative_versions)
}

fn find_main_audio_book_file(files: &[AudioBookFileInfo], name: &str) -> AudioBookFileInfo {
    if let Some(m) = files
        .iter()
        .find(|x| path::file_name_without_extension(&x.path).eq_ignore_ascii_case(name))
    {
        return m.clone();
    }
    if let Some(m) = files
        .iter()
        .find(|x| path::file_name_without_extension(&x.path).eq_ignore_ascii_case("audiobook"))
    {
        return m.clone();
    }
    // OrderBy(Container).ThenBy(Path).First()
    files
        .iter()
        .min_by(|a, b| {
            a.container
                .cmp(&b.container)
                .then_with(|| a.path.cmp(&b.path))
        })
        .cloned()
        .expect("find_main_audio_book_file called on non-empty list")
}

fn remove_all(stack_files: &mut Vec<AudioBookFileInfo>, to_remove: &[AudioBookFileInfo]) {
    stack_files.retain(|f| !to_remove.contains(f));
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}
