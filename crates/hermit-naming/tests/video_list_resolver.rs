//! Ported from `Video/VideoListResolverTests.cs`.

use hermit_model::entities::ExtraType;
use hermit_naming::common::NamingOptions;
use hermit_naming::video::{FileStack, VideoFileInfo, VideoListResolver, video_resolver};

/// Resolves the file list into `VideoFileInfo`s (mirrors the C# per-file
/// `VideoResolver.Resolve(..., false)` + `OfType<VideoFileInfo>`).
fn resolve_infos(
    files: &[&str],
    is_directory: bool,
    options: &NamingOptions,
) -> Vec<VideoFileInfo> {
    files
        .iter()
        .filter_map(|i| video_resolver::resolve(Some(i), is_directory, options, true, None))
        .collect()
}

#[test]
fn test_stack_and_extras() {
    let options = NamingOptions::new();
    let resolver = VideoListResolver::new(&options);
    let infos = resolve_infos(
        &[
            "Harry Potter and the Deathly Hallows-trailer.mkv",
            "Harry Potter and the Deathly Hallows.trailer.mkv",
            "Harry Potter and the Deathly Hallows part1.mkv",
            "Harry Potter and the Deathly Hallows part2.mkv",
            "Harry Potter and the Deathly Hallows part3.mkv",
            "Harry Potter and the Deathly Hallows part4.mkv",
            "Batman-deleted.mkv",
            "Batman-sample.mkv",
            "Batman-trailer.mkv",
            "Batman part1.mkv",
            "Batman part2.mkv",
            "Batman part3.mkv",
            "Avengers.mkv",
            "Avengers-trailer.mkv",
            "trailer.mkv",
            "WillyWonka-trailer.mkv",
        ],
        false,
        &options,
    );
    let result = resolver.resolve_simple(&infos);

    assert_eq!(result.len(), 11);
    let batman = result
        .iter()
        .find(|x| x.name.as_deref() == Some("Batman"))
        .expect("batman present");
    assert_eq!(batman.files.len(), 3);

    let harry = result
        .iter()
        .find(|x| x.name.as_deref() == Some("Harry Potter and the Deathly Hallows"))
        .expect("harry present");
    assert_eq!(harry.files.len(), 4);

    assert!(result[2].extra_type.is_none());
    assert_eq!(result[3].extra_type, Some(ExtraType::Trailer));
    assert_eq!(result[4].extra_type, Some(ExtraType::Trailer));
    assert_eq!(result[5].extra_type, Some(ExtraType::DeletedScene));
    assert_eq!(result[6].extra_type, Some(ExtraType::Sample));
    assert_eq!(result[7].extra_type, Some(ExtraType::Trailer));
    assert_eq!(result[8].extra_type, Some(ExtraType::Trailer));
    assert_eq!(result[9].extra_type, Some(ExtraType::Trailer));
    assert_eq!(result[10].extra_type, Some(ExtraType::Trailer));
}

fn resolve_files_default(
    files: &[&str],
    is_directory: bool,
) -> Vec<hermit_naming::video::VideoInfo> {
    let options = NamingOptions::new();
    let resolver = VideoListResolver::new(&options);
    let infos = resolve_infos(files, is_directory, &options);
    resolver.resolve_simple(&infos)
}

#[test]
fn test_with_metadata() {
    let result = resolve_files_default(&["300.mkv", "300.nfo"], false);
    assert_eq!(result.len(), 1);
}

#[test]
fn test_with_extra() {
    let result = resolve_files_default(&["300.mkv", "300 - trailer.mkv"], false);
    assert_eq!(result.len(), 2);
    assert!(result[0].extra_type.is_none());
    assert_eq!(result[1].extra_type, Some(ExtraType::Trailer));
}

#[test]
fn test_variation_with_folder_name() {
    let result = resolve_files_default(
        &[
            "X-Men Days of Future Past - 1080p.mkv",
            "X-Men Days of Future Past-trailer.mp4",
        ],
        false,
    );
    assert_eq!(result.len(), 2);
    assert!(result[0].extra_type.is_none());
    assert_eq!(result[1].extra_type, Some(ExtraType::Trailer));
}

#[test]
fn test_trailer2() {
    let result = resolve_files_default(
        &[
            "X-Men Days of Future Past - 1080p.mkv",
            "X-Men Days of Future Past-trailer.mp4",
            "X-Men Days of Future Past-trailer2.mp4",
        ],
        false,
    );
    assert_eq!(result.len(), 3);
    assert!(result[0].extra_type.is_none());
    assert_eq!(result[1].extra_type, Some(ExtraType::Trailer));
    assert_eq!(result[2].extra_type, Some(ExtraType::Trailer));
}

#[test]
fn resolve_same_name_and_year_returns_single_item() {
    let result = resolve_files_default(
        &[
            "Looper (2012)-trailer.mkv",
            "Looper 2012-trailer.mkv",
            "Looper.2012.bluray.720p.x264.mkv",
        ],
        false,
    );
    assert_eq!(result.len(), 3);
    assert!(result[0].extra_type.is_none());
    assert_eq!(result[1].extra_type, Some(ExtraType::Trailer));
    assert_eq!(result[2].extra_type, Some(ExtraType::Trailer));
}

#[test]
fn resolve_trailer_matches_folder_name_returns_single_item() {
    let result = resolve_files_default(
        &[
            "/movies/Looper (2012)/Looper (2012)-trailer.mkv",
            "/movies/Looper (2012)/Looper.bluray.720p.x264.mkv",
        ],
        false,
    );
    assert_eq!(result.len(), 2);
    assert!(result[0].extra_type.is_none());
    assert_eq!(result[1].extra_type, Some(ExtraType::Trailer));
}

#[test]
fn test_separate_files() {
    let result = resolve_files_default(
        &[
            "My video 1.mkv",
            "My video 2.mkv",
            "My video 3.mkv",
            "My video 4.mkv",
            "My video 5.mkv",
        ],
        false,
    );
    assert_eq!(result.len(), 5);
}

#[test]
fn test_multi_disc() {
    let result = resolve_files_default(
        &[
            "M:/Movies (DVD)/Movies (Musical)/Sound of Music (1965)/Sound of Music Disc 1",
            "M:/Movies (DVD)/Movies (Musical)/Sound of Music (1965)/Sound of Music Disc 2",
        ],
        true,
    );
    assert_eq!(result.len(), 1);
}

#[test]
fn test_pound_sign() {
    let result = resolve_files_default(&["My movie #1.mp4", "My movie #2.mp4"], true);
    assert_eq!(result.len(), 2);
}

#[test]
fn test_stacked_with_trailer() {
    let result = resolve_files_default(
        &[
            "No (2012) part1.mp4",
            "No (2012) part2.mp4",
            "No (2012) part1-trailer.mp4",
            "No (2012)-trailer.mp4",
        ],
        false,
    );
    assert_eq!(result.len(), 3);
    assert!(result[0].extra_type.is_none());
    assert_eq!(result[1].extra_type, Some(ExtraType::Trailer));
    assert_eq!(result[2].extra_type, Some(ExtraType::Trailer));
}

#[test]
fn test_extras_by_folder_name() {
    let result = resolve_files_default(
        &[
            "/Movies/Top Gun (1984)/movie.mp4",
            "/Movies/Top Gun (1984)/Top Gun (1984)-trailer.mp4",
            "/Movies/Top Gun (1984)/Top Gun (1984)-trailer2.mp4",
            "/Movies/trailer.mp4",
        ],
        false,
    );
    assert_eq!(result.len(), 4);
    assert!(result[0].extra_type.is_none());
    assert_eq!(result[1].extra_type, Some(ExtraType::Trailer));
    assert_eq!(result[2].extra_type, Some(ExtraType::Trailer));
    assert_eq!(result[3].extra_type, Some(ExtraType::Trailer));
}

#[test]
fn test_double_tags() {
    let result = resolve_files_default(
        &[
            "/MCFAMILY-PC/Private3$/Heterosexual/Breast In Class 2 Counterfeit Racks (2011)/Breast In Class 2 Counterfeit Racks (2011) Disc 1 cd1.avi",
            "/MCFAMILY-PC/Private3$/Heterosexual/Breast In Class 2 Counterfeit Racks (2011)/Breast In Class 2 Counterfeit Racks (2011) Disc 1 cd2.avi",
            "/MCFAMILY-PC/Private3$/Heterosexual/Breast In Class 2 Counterfeit Racks (2011)/Breast In Class 2 Disc 2 cd1.avi",
            "/MCFAMILY-PC/Private3$/Heterosexual/Breast In Class 2 Counterfeit Racks (2011)/Breast In Class 2 Disc 2 cd2.avi",
        ],
        false,
    );
    assert_eq!(result.len(), 2);
}

#[test]
fn test_argument_out_of_range_exception() {
    let result = resolve_files_default(
        &["/nas-markrobbo78/Videos/INDEX HTPC/Movies/Watched/3 - ACTION/Argo (2012)/movie.mkv"],
        false,
    );
    assert_eq!(result.len(), 1);
}

#[test]
fn test_colony() {
    let result = resolve_files_default(&["The Colony.mkv"], false);
    assert_eq!(result.len(), 1);
}

#[test]
fn test_four_sisters() {
    let result = resolve_files_default(
        &[
            "Four Sisters and a Wedding - A.avi",
            "Four Sisters and a Wedding - B.avi",
        ],
        false,
    );
    assert_eq!(result.len(), 2);
}

#[test]
fn test_four_rooms() {
    let result = resolve_files_default(&["Four Rooms - A.avi", "Four Rooms - A.mp4"], false);
    assert_eq!(result.len(), 2);
}

#[test]
fn test_movie_trailer() {
    let result = resolve_files_default(
        &[
            "/Server/Despicable Me/Despicable Me (2010).mkv",
            "/Server/Despicable Me/trailer.mkv",
        ],
        false,
    );
    assert_eq!(result.len(), 2);
    assert!(result[0].extra_type.is_none());
    assert_eq!(result[1].extra_type, Some(ExtraType::Trailer));
}

#[test]
fn resolve_trailer_in_trailers_folder_returns_correct_extra_type() {
    let result = resolve_files_default(
        &[
            "/Server/Despicable Me/Despicable Me (2010).mkv",
            "/Server/Despicable Me/trailers/some title.mkv",
        ],
        false,
    );
    assert_eq!(result.len(), 2);
    assert!(result[0].extra_type.is_none());
    assert_eq!(result[1].extra_type, Some(ExtraType::Trailer));
}

#[test]
fn test_subfolders() {
    let result = resolve_files_default(
        &[
            "/Movies/Despicable Me/Despicable Me.mkv",
            "/Movies/Despicable Me/trailers/trailer.mkv",
        ],
        false,
    );
    assert_eq!(result.len(), 2);
    assert!(result[0].extra_type.is_none());
    assert_eq!(result[1].extra_type, Some(ExtraType::Trailer));
}

#[test]
fn test_directory_stack() {
    let stack = FileStack::new(String::new(), false, Vec::new());
    assert!(!stack.contains_file("XX", true));
}
