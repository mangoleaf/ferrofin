//! Ported from `AudioBook/AudioBookListResolverTests.cs`.

use hermit_naming::audiobook::AudioBookListResolver;
use hermit_naming::common::NamingOptions;
use hermit_naming::io::FileSystemMetadata;

fn metas(files: &[&str]) -> Vec<FileSystemMetadata> {
    files
        .iter()
        .map(|f| FileSystemMetadata::new(*f, false))
        .collect()
}

#[test]
fn test_stack_and_extras() {
    let options = NamingOptions::new();
    let files = metas(&[
        "Harry Potter and the Deathly Hallows/Part 1.mp3",
        "Harry Potter and the Deathly Hallows/Part 2.mp3",
        "Harry Potter and the Deathly Hallows/Extra.mp3",
        "Batman/Chapter 1.mp3",
        "Batman/Chapter 2.mp3",
        "Batman/Chapter 3.mp3",
        "Badman/audiobook.mp3",
        "Badman/extra.mp3",
        "Superman (2020)/Part 1.mp3",
        "Superman (2020)/extra.mp3",
        "Ready Player One (2020)/audiobook.mp3",
        "Ready Player One (2020)/extra.mp3",
        ".mp3",
    ]);

    let result = AudioBookListResolver::new(&options).resolve(&files);

    assert_eq!(result.len(), 5);

    assert_eq!(result[0].files.len(), 2);
    assert_eq!(result[0].extras.len(), 1);
    assert_eq!(result[0].name, "Harry Potter and the Deathly Hallows");

    assert_eq!(result[1].files.len(), 3);
    assert!(result[1].extras.is_empty());
    assert_eq!(result[1].name, "Batman");

    assert_eq!(result[2].files.len(), 1);
    assert_eq!(result[2].extras.len(), 1);
    assert_eq!(result[2].name, "Badman");

    assert_eq!(result[3].files.len(), 1);
    assert_eq!(result[3].extras.len(), 1);
    assert_eq!(result[3].name, "Superman");

    assert_eq!(result[4].files.len(), 1);
    assert_eq!(result[4].extras.len(), 1);
    assert_eq!(result[4].name, "Ready Player One");
}

#[test]
fn test_alternative_versions() {
    let options = NamingOptions::new();
    let files = metas(&[
        "Harry Potter and the Deathly Hallows/Chapter 1.ogg",
        "Harry Potter and the Deathly Hallows/Chapter 1.mp3",
        "Deadpool.mp3",
        "Deadpool [HQ].mp3",
        "Superman/audiobook.mp3",
        "Superman/Superman.mp3",
        "Superman/Superman [HQ].mp3",
        "Superman/extra.mp3",
        "Batman/ Chapter 1 .mp3",
        "Batman/Chapter 1[loss-less].mp3",
    ]);

    let result = AudioBookListResolver::new(&options).resolve(&files);

    assert_eq!(result.len(), 5);
    assert_eq!(result[0].alternate_versions.len(), 1);
    assert!(result[1].alternate_versions.is_empty());
    assert!(result[2].alternate_versions.is_empty());
    assert_eq!(result[3].alternate_versions.len(), 2);
    let paths: Vec<&str> = result[3]
        .alternate_versions
        .iter()
        .map(|x| x.path.as_str())
        .collect();
    assert!(paths.contains(&"Superman/audiobook.mp3"));
    assert!(paths.contains(&"Superman/Superman [HQ].mp3"));
    assert_eq!(result[4].alternate_versions.len(), 1);
}

#[test]
fn test_name_year_extraction() {
    let options = NamingOptions::new();
    // (name, path, year)
    let data: [(&str, &str, Option<i32>); 7] = [
        (
            "Harry Potter and the Deathly Hallows",
            "Harry Potter and the Deathly Hallows (2007)/Chapter 1.ogg",
            Some(2007),
        ),
        ("Batman", "Batman (2020).ogg", Some(2020)),
        ("Batman", "Batman( 2021 ).mp3", Some(2021)),
        ("Batman(*2021*)", "Batman(*2021*).mp3", None),
        ("Batman", "Batman.mp3", None),
        ("+ Batman .", " + Batman . .mp3", None),
        (" ", " .mp3", None),
    ];

    let files = metas(&data.iter().map(|(_, p, _)| *p).collect::<Vec<_>>());
    let result = AudioBookListResolver::new(&options).resolve(&files);

    assert_eq!(result.len(), data.len());
    for (i, (name, _, year)) in data.iter().enumerate() {
        assert_eq!(&result[i].name, name, "name at {i}");
        assert_eq!(result[i].year, *year, "year at {i}");
    }
}

#[test]
fn test_with_metadata() {
    let options = NamingOptions::new();
    let files = metas(&[
        "Harry Potter and the Deathly Hallows/Chapter 1.ogg",
        "Harry Potter and the Deathly Hallows/Harry Potter and the Deathly Hallows.nfo",
    ]);
    let result = AudioBookListResolver::new(&options).resolve(&files);
    assert_eq!(result.len(), 1);
}

#[test]
fn test_with_extra() {
    let options = NamingOptions::new();
    let files = metas(&[
        "Harry Potter and the Deathly Hallows/Chapter 1.mp3",
        "Harry Potter and the Deathly Hallows/Harry Potter and the Deathly Hallows trailer.mp3",
    ]);
    let result = AudioBookListResolver::new(&options).resolve(&files);
    assert_eq!(result.len(), 1);
}

#[test]
fn test_without_folder() {
    let options = NamingOptions::new();
    let files = metas(&["Harry Potter and the Deathly Hallows trailer.mp3"]);
    let result = AudioBookListResolver::new(&options).resolve(&files);
    assert_eq!(result.len(), 1);
}

#[test]
fn test_empty() {
    let options = NamingOptions::new();
    let result = AudioBookListResolver::new(&options).resolve(&[]);
    assert!(result.is_empty());
}
