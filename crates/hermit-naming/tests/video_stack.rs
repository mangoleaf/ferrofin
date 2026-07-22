//! Ported from `Video/StackTests.cs`.

use hermit_naming::common::NamingOptions;
use hermit_naming::io::FileSystemMetadata;
use hermit_naming::video::{FileStack, stack_resolver};

fn strs(files: &[&str]) -> Vec<String> {
    files.iter().map(|s| (*s).to_string()).collect()
}

fn test_stack_info(stack: &FileStack, name: &str, file_count: usize) {
    assert_eq!(stack.files.len(), file_count);
    assert_eq!(stack.name, name);
}

#[test]
fn test_simple_stack() {
    let options = NamingOptions::new();
    let files = strs(&[
        "Bad Boys (2006) part1.mkv",
        "Bad Boys (2006) part2.mkv",
        "Bad Boys (2006) part3.mkv",
        "Bad Boys (2006) part4.mkv",
        "Bad Boys (2006)-trailer.mkv",
    ]);
    let result = stack_resolver::resolve_files(&files, &options);
    assert_eq!(result.len(), 1);
    test_stack_info(&result[0], "Bad Boys (2006)", 4);
}

#[test]
fn test_false_positives() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&["Bad Boys (2006).mkv", "Bad Boys (2007).mkv"]),
        &options,
    );
    assert!(result.is_empty());
}

#[test]
fn test_false_positives2() {
    let options = NamingOptions::new();
    let result =
        stack_resolver::resolve_files(&strs(&["Bad Boys 2006.mkv", "Bad Boys 2007.mkv"]), &options);
    assert!(result.is_empty());
}

#[test]
fn test_false_positives3() {
    let options = NamingOptions::new();
    let result =
        stack_resolver::resolve_files(&strs(&["300 (2006).mkv", "300 (2007).mkv"]), &options);
    assert!(result.is_empty());
}

#[test]
fn test_false_positives4() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(&strs(&["300 2006.mkv", "300 2007.mkv"]), &options);
    assert!(result.is_empty());
}

#[test]
fn test_false_positives5() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "Star Trek 1 - The motion picture.mkv",
            "Star Trek 2- The wrath of khan.mkv",
        ]),
        &options,
    );
    assert!(result.is_empty());
}

#[test]
fn test_false_positives6() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "Red Riding in the Year of Our Lord 1983 (2009).mkv",
            "Red Riding in the Year of Our Lord 1980 (2009).mkv",
            "Red Riding in the Year of Our Lord 1974 (2009).mkv",
        ]),
        &options,
    );
    assert!(result.is_empty());
}

#[test]
fn test_stack_name() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "d:/movies/300 2006 part1.mkv",
            "d:/movies/300 2006 part2.mkv",
        ]),
        &options,
    );
    assert_eq!(result.len(), 1);
    test_stack_info(&result[0], "300 2006", 2);
}

#[test]
fn resolve_files_given_part_in_middle_of_name_returns_no_stack() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "Bad Boys (2006).part1.stv.unrated.multi.1080p.bluray.x264-rough.mkv",
            "Bad Boys (2006).part2.stv.unrated.multi.1080p.bluray.x264-rough.mkv",
            "Bad Boys (2006).part3.stv.unrated.multi.1080p.bluray.x264-rough.mkv",
            "Bad Boys (2006).part4.stv.unrated.multi.1080p.bluray.x264-rough.mkv",
            "Bad Boys (2006)-trailer.mkv",
        ]),
        &options,
    );
    assert!(result.is_empty());
}

#[test]
fn resolve_files_file_names_with_missing_part_type_returns_no_stack() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "Bad Boys (2006).mkv",
            "Bad Boys (2006) 1.mkv",
            "Bad Boys (2006) 2.mkv",
            "Bad Boys (2006) 3.mkv",
            "Bad Boys (2006)-trailer.mkv",
        ]),
        &options,
    );
    assert!(result.is_empty());
}

#[test]
fn test_simple_stack_with_numeric_name() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "300 (2006) part1.mkv",
            "300 (2006) part2.mkv",
            "300 (2006) part3.mkv",
            "300 (2006) part4.mkv",
            "300 (2006)-trailer.mkv",
        ]),
        &options,
    );
    assert_eq!(result.len(), 1);
    test_stack_info(&result[0], "300 (2006)", 4);
}

#[test]
fn test_mixed_expressions_not_allowed() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "Bad Boys (2006) part1.mkv",
            "Bad Boys (2006) part2.mkv",
            "Bad Boys (2006) part3.mkv",
            "Bad Boys (2006) parta.mkv",
            "Bad Boys (2006)-trailer.mkv",
        ]),
        &options,
    );
    assert_eq!(result.len(), 1);
    test_stack_info(&result[0], "Bad Boys (2006)", 3);
}

#[test]
fn test_dual_stacks() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "Bad Boys (2006) part1.mkv",
            "Bad Boys (2006) part2.mkv",
            "Bad Boys (2006) part3.mkv",
            "Bad Boys (2006) part4.mkv",
            "Bad Boys (2006)-trailer.mkv",
            "300 (2006) part1.mkv",
            "300 (2006) part2.mkv",
            "300 (2006) part3.mkv",
            "300 (2006)-trailer.mkv",
        ]),
        &options,
    );
    assert_eq!(result.len(), 2);
    test_stack_info(&result[1], "Bad Boys (2006)", 4);
    test_stack_info(&result[0], "300 (2006)", 3);
}

#[test]
fn test_directories() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_directories(
        &strs(&["blah blah - cd 1", "blah blah - cd 2"]),
        &options,
    );
    assert_eq!(result.len(), 1);
    test_stack_info(&result[0], "blah blah", 2);
}

#[test]
fn test_missing_parttype() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&["300a.mkv", "300b.mkv", "300c.mkv", "300-trailer.mkv"]),
        &options,
    );
    assert!(result.is_empty());
}

#[test]
fn test_fail_sequence() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "300 part1.mkv",
            "300 part2.mkv",
            "Avatar",
            "Avengers part1.mkv",
            "Avengers part2.mkv",
            "Avengers part3.mkv",
        ]),
        &options,
    );
    assert_eq!(result.len(), 2);
    test_stack_info(&result[0], "300", 2);
    test_stack_info(&result[1], "Avengers", 3);
}

#[test]
fn test_mixed_expressions() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "Bad Boys (2006) part1.mkv",
            "Bad Boys (2006) part2.mkv",
            "Bad Boys (2006) part3.mkv",
            "Bad Boys (2006) part4.mkv",
            "Bad Boys (2006)-trailer.mkv",
            "300 (2006) parta.mkv",
            "300 (2006) partb.mkv",
            "300 (2006) partc.mkv",
            "300 (2006) partd.mkv",
            "300 (2006)-trailer.mkv",
            "300a.mkv",
            "300b.mkv",
            "300c.mkv",
            "300-trailer.mkv",
        ]),
        &options,
    );
    assert_eq!(result.len(), 2);
    test_stack_info(&result[0], "300 (2006)", 4);
    test_stack_info(&result[1], "Bad Boys (2006)", 4);
}

#[test]
fn test_alpha_limit_of_four() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "300 (2006) parta.mkv",
            "300 (2006) partb.mkv",
            "300 (2006) partc.mkv",
            "300 (2006) partd.mkv",
            "300 (2006) parte.mkv",
            "300 (2006) partf.mkv",
            "300 (2006) partg.mkv",
            "300 (2006)-trailer.mkv",
        ]),
        &options,
    );
    assert_eq!(result.len(), 1);
    test_stack_info(&result[0], "300 (2006)", 4);
}

#[test]
fn test_mixed() {
    let options = NamingOptions::new();
    let files = vec![
        FileSystemMetadata::new("Bad Boys (2006) part1.mkv", false),
        FileSystemMetadata::new("Bad Boys (2006) part2.mkv", false),
        FileSystemMetadata::new("300 (2006) part2", true),
        FileSystemMetadata::new("300 (2006) part3", true),
        FileSystemMetadata::new("300 (2006) part1", true),
    ];
    let result = stack_resolver::resolve(&files, &options);
    assert_eq!(result.len(), 2);
    test_stack_info(&result[0], "300 (2006)", 3);
    test_stack_info(&result[1], "Bad Boys (2006)", 2);
}

#[test]
fn test_names_without_parts() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "Harry Potter and the Deathly Hallows.mkv",
            "Harry Potter and the Deathly Hallows 1.mkv",
            "Harry Potter and the Deathly Hallows 2.mkv",
            "Harry Potter and the Deathly Hallows 3.mkv",
            "Harry Potter and the Deathly Hallows 4.mkv",
        ]),
        &options,
    );
    assert!(result.is_empty());
}

#[test]
fn test_numbers_appearing_before_part_number() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_files(
        &strs(&[
            "Neverland (2011)[720p][PG][Voted 6.5][Family-Fantasy]part1.mkv",
            "Neverland (2011)[720p][PG][Voted 6.5][Family-Fantasy]part2.mkv",
        ]),
        &options,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
}

#[test]
fn test_multi_discs() {
    let options = NamingOptions::new();
    let result = stack_resolver::resolve_directories(
        &strs(&[
            "M:/Movies (DVD)/Movies (Musical)/The Sound of Music/The Sound of Music (1965) (Disc 01)",
            "M:/Movies (DVD)/Movies (Musical)/The Sound of Music/The Sound of Music (1965) (Disc 02)",
        ]),
        &options,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
}
