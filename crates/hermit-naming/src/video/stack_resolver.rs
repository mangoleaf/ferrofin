//! Port of `Emby.Naming.Video.StackResolver`.

use crate::audiobook::AudioBookFileInfo;
use crate::common::NamingOptions;
use crate::io::FileSystemMetadata;
use crate::path;
use crate::video::{FileStack, is_stub_file, is_video_file};

/// Resolves only directories from paths.
#[must_use]
pub fn resolve_directories(files: &[String], naming_options: &NamingOptions) -> Vec<FileStack> {
    let metadata: Vec<FileSystemMetadata> = files
        .iter()
        .map(|i| FileSystemMetadata::new(i.clone(), true))
        .collect();
    resolve(&metadata, naming_options)
}

/// Resolves only files from paths.
#[must_use]
pub fn resolve_files(files: &[String], naming_options: &NamingOptions) -> Vec<FileStack> {
    let metadata: Vec<FileSystemMetadata> = files
        .iter()
        .map(|i| FileSystemMetadata::new(i.clone(), false))
        .collect();
    resolve(&metadata, naming_options)
}

/// Resolves audiobooks from paths (grouped by directory).
#[must_use]
pub fn resolve_audio_books(files: &[AudioBookFileInfo]) -> Vec<FileStack> {
    // Group by directory, preserving first-seen directory order.
    let mut order: Vec<Option<String>> = Vec::new();
    let mut groups: Vec<(Option<String>, Vec<String>)> = Vec::new();

    for file in files {
        let dir = path::directory_name(&file.path).map(str::to_string);
        if let Some(pos) = order.iter().position(|d| d == &dir) {
            groups[pos].1.push(file.path.clone());
        } else {
            order.push(dir.clone());
            groups.push((dir, vec![file.path.clone()]));
        }
    }

    let mut result = Vec::new();
    for (dir, paths) in groups {
        match dir {
            None => {
                for p in paths {
                    let name = path::file_name_without_extension(&p).to_string();
                    result.push(FileStack::new(name, false, vec![p]));
                }
            }
            Some(dir) => {
                result.push(FileStack::new(path::file_name(&dir), false, paths));
            }
        }
    }

    result
}

/// Resolves videos from paths.
#[must_use]
pub fn resolve(files: &[FileSystemMetadata], naming_options: &NamingOptions) -> Vec<FileStack> {
    let mut potential_files: Vec<&FileSystemMetadata> = files
        .iter()
        .filter(|i| {
            i.is_directory
                || is_video_file(&i.full_name, naming_options)
                || is_stub_file(&i.full_name, naming_options)
        })
        .collect();
    potential_files.sort_by(|a, b| a.full_name.cmp(&b.full_name));

    // Insertion-ordered map of stack name → metadata.
    let mut order: Vec<String> = Vec::new();
    let mut stacks: Vec<StackMetadata> = Vec::new();

    for file in potential_files {
        let mut name = file.name();
        if name.is_empty() {
            name = path::file_name(&file.full_name);
        }

        for rule in &naming_options.video_file_stacking_rules {
            let Some(parsed) = rule.match_input(name) else {
                continue;
            };

            let stack_name = parsed.stack_name;
            let part_number = parsed.part_number;
            let part_type = parsed.part_type;

            let idx = if let Some(pos) = order.iter().position(|n| n == &stack_name) {
                pos
            } else {
                order.push(stack_name);
                stacks.push(StackMetadata::new(
                    file.is_directory,
                    rule.is_numerical,
                    part_type.clone(),
                ));
                stacks.len() - 1
            };

            if !stacks[idx].parts.is_empty() {
                if stacks[idx].is_directory != file.is_directory
                    || !part_type.eq_ignore_ascii_case(&stacks[idx].part_type)
                    || stacks[idx].contains_part(&part_number)
                {
                    continue;
                }

                if rule.is_numerical != stacks[idx].is_numerical {
                    break;
                }
            }

            stacks[idx].parts.push((part_number, file.clone()));
            break;
        }
    }

    let mut result = Vec::new();
    for (name, stack) in order.into_iter().zip(stacks) {
        if stack.parts.len() < 2 {
            continue;
        }
        let files = stack.parts.into_iter().map(|(_, f)| f.full_name).collect();
        result.push(FileStack::new(name, stack.is_directory, files));
    }

    result
}

struct StackMetadata {
    /// Ordered (part number → metadata) pairs; keys are case-insensitive.
    parts: Vec<(String, FileSystemMetadata)>,
    is_directory: bool,
    is_numerical: bool,
    part_type: String,
}

impl StackMetadata {
    fn new(is_directory: bool, is_numerical: bool, part_type: String) -> Self {
        Self {
            parts: Vec::new(),
            is_directory,
            is_numerical,
            part_type,
        }
    }

    fn contains_part(&self, part_number: &str) -> bool {
        self.parts
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(part_number))
    }
}
