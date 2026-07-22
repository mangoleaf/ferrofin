//! Ported from `Video/MultiVersionTests.cs`.

use hermit_model::data::CollectionType;
use hermit_model::entities::ExtraType;
use hermit_naming::common::NamingOptions;
use hermit_naming::video::{VideoFileInfo, VideoInfo, VideoListResolver, video_resolver};

fn infos(files: &[&str], options: &NamingOptions) -> Vec<VideoFileInfo> {
    files
        .iter()
        .filter_map(|i| video_resolver::resolve(Some(i), false, options, true, None))
        .collect()
}

fn resolve(files: &[&str]) -> Vec<VideoInfo> {
    let options = NamingOptions::new();
    let resolver = VideoListResolver::new(&options);
    resolver.resolve_simple(&infos(files, &options))
}

fn resolve_tv(files: &[&str]) -> Vec<VideoInfo> {
    let options = NamingOptions::new();
    let resolver = VideoListResolver::new(&options);
    resolver.resolve(
        &infos(files, &options),
        true,
        true,
        None,
        Some(CollectionType::tvshows),
    )
}

fn primary_path(v: &VideoInfo) -> &str {
    &v.files[0].path
}

#[test]
fn test_multi_edition1() {
    let result = resolve(&[
        "/movies/X-Men Days of Future Past/X-Men Days of Future Past - 1080p.mkv",
        "/movies/X-Men Days of Future Past/X-Men Days of Future Past-trailer.mp4",
        "/movies/X-Men Days of Future Past/X-Men Days of Future Past - [hsbs].mkv",
        "/movies/X-Men Days of Future Past/X-Men Days of Future Past [hsbs].mkv",
    ]);
    assert_eq!(result.iter().filter(|v| v.extra_type.is_none()).count(), 1);
    assert_eq!(result.iter().filter(|v| v.extra_type.is_some()).count(), 1);
}

#[test]
fn test_multi_edition2() {
    let result = resolve(&[
        "/movies/X-Men Days of Future Past/X-Men Days of Future Past - apple.mkv",
        "/movies/X-Men Days of Future Past/X-Men Days of Future Past-trailer.mp4",
        "/movies/X-Men Days of Future Past/X-Men Days of Future Past - banana.mkv",
        "/movies/X-Men Days of Future Past/X-Men Days of Future Past [banana].mp4",
    ]);
    assert_eq!(result.iter().filter(|v| v.extra_type.is_none()).count(), 1);
    assert_eq!(result.iter().filter(|v| v.extra_type.is_some()).count(), 1);
    assert_eq!(result[0].alternate_versions.len(), 2);
}

#[test]
fn test_multi_edition3() {
    let result = resolve(&[
        "/movies/The Phantom of the Opera (1925)/The Phantom of the Opera (1925) - 1925 version.mkv",
        "/movies/The Phantom of the Opera (1925)/The Phantom of the Opera (1925) - 1929 version.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 1);
}

#[test]
fn test_letter_folders() {
    let result = resolve(&[
        "/movies/M/Movie 1.mkv",
        "/movies/M/Movie 2.mkv",
        "/movies/M/Movie 3.mkv",
        "/movies/M/Movie 4.mkv",
        "/movies/M/Movie 5.mkv",
        "/movies/M/Movie 6.mkv",
        "/movies/M/Movie 7.mkv",
    ]);
    assert_eq!(result.len(), 7);
    assert!(result[0].alternate_versions.is_empty());
}

#[test]
fn test_multi_version_limit() {
    let result = resolve(&[
        "/movies/Movie/Movie.mkv",
        "/movies/Movie/Movie-2.mkv",
        "/movies/Movie/Movie-3.mkv",
        "/movies/Movie/Movie-4.mkv",
        "/movies/Movie/Movie-5.mkv",
        "/movies/Movie/Movie-6.mkv",
        "/movies/Movie/Movie-7.mkv",
        "/movies/Movie/Movie-8.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 7);
}

#[test]
fn test_multi_version_limit2() {
    let result = resolve(&[
        "/movies/Mo/Movie 1.mkv",
        "/movies/Mo/Movie 2.mkv",
        "/movies/Mo/Movie 3.mkv",
        "/movies/Mo/Movie 4.mkv",
        "/movies/Mo/Movie 5.mkv",
        "/movies/Mo/Movie 6.mkv",
        "/movies/Mo/Movie 7.mkv",
        "/movies/Mo/Movie 8.mkv",
        "/movies/Mo/Movie 9.mkv",
    ]);
    assert_eq!(result.len(), 9);
    assert!(result[0].alternate_versions.is_empty());
}

#[test]
fn test_multi_version3() {
    let result = resolve(&[
        "/movies/Movie/Movie 1.mkv",
        "/movies/Movie/Movie 2.mkv",
        "/movies/Movie/Movie 3.mkv",
        "/movies/Movie/Movie 4.mkv",
        "/movies/Movie/Movie 5.mkv",
    ]);
    assert_eq!(result.len(), 5);
    assert!(result[0].alternate_versions.is_empty());
}

#[test]
fn test_multi_version4() {
    let result = resolve(&[
        "/movies/Iron Man/Iron Man.mkv",
        "/movies/Iron Man/Iron Man (2008).mkv",
        "/movies/Iron Man/Iron Man (2009).mkv",
        "/movies/Iron Man/Iron Man (2010).mkv",
        "/movies/Iron Man/Iron Man (2011).mkv",
    ]);
    assert_eq!(result.len(), 5);
    assert!(result[0].alternate_versions.is_empty());
}

#[test]
fn test_multi_version5() {
    let result = resolve(&[
        "/movies/Iron Man/Iron Man.mkv",
        "/movies/Iron Man/Iron Man-720p.mkv",
        "/movies/Iron Man/Iron Man-test.mkv",
        "/movies/Iron Man/Iron Man-bluray.mkv",
        "/movies/Iron Man/Iron Man-3d.mkv",
        "/movies/Iron Man/Iron Man-3d-hsbs.mkv",
        "/movies/Iron Man/Iron Man[test].mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(primary_path(&result[0]), "/movies/Iron Man/Iron Man.mkv");
    assert_eq!(result[0].alternate_versions.len(), 6);
    assert_eq!(
        primary_path(&result[0].alternate_versions[0]),
        "/movies/Iron Man/Iron Man-720p.mkv"
    );
    assert_eq!(
        primary_path(&result[0].alternate_versions[1]),
        "/movies/Iron Man/Iron Man-3d.mkv"
    );
    assert_eq!(
        primary_path(&result[0].alternate_versions[2]),
        "/movies/Iron Man/Iron Man-3d-hsbs.mkv"
    );
    assert_eq!(
        primary_path(&result[0].alternate_versions[3]),
        "/movies/Iron Man/Iron Man-bluray.mkv"
    );
    assert_eq!(
        primary_path(&result[0].alternate_versions[4]),
        "/movies/Iron Man/Iron Man-test.mkv"
    );
    assert_eq!(
        primary_path(&result[0].alternate_versions[5]),
        "/movies/Iron Man/Iron Man[test].mkv"
    );
}

#[test]
fn test_multi_version6() {
    let result = resolve(&[
        "/movies/Iron Man/Iron Man.mkv",
        "/movies/Iron Man/Iron Man - 720p.mkv",
        "/movies/Iron Man/Iron Man - test.mkv",
        "/movies/Iron Man/Iron Man - bluray.mkv",
        "/movies/Iron Man/Iron Man - 3d.mkv",
        "/movies/Iron Man/Iron Man - 3d-hsbs.mkv",
        "/movies/Iron Man/Iron Man [test].mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(primary_path(&result[0]), "/movies/Iron Man/Iron Man.mkv");
    assert_eq!(result[0].alternate_versions.len(), 6);
    assert_eq!(
        primary_path(&result[0].alternate_versions[0]),
        "/movies/Iron Man/Iron Man - 720p.mkv"
    );
    assert_eq!(
        primary_path(&result[0].alternate_versions[1]),
        "/movies/Iron Man/Iron Man - 3d.mkv"
    );
    assert_eq!(
        primary_path(&result[0].alternate_versions[2]),
        "/movies/Iron Man/Iron Man - 3d-hsbs.mkv"
    );
    assert_eq!(
        primary_path(&result[0].alternate_versions[3]),
        "/movies/Iron Man/Iron Man - bluray.mkv"
    );
    assert_eq!(
        primary_path(&result[0].alternate_versions[4]),
        "/movies/Iron Man/Iron Man - test.mkv"
    );
    assert_eq!(
        primary_path(&result[0].alternate_versions[5]),
        "/movies/Iron Man/Iron Man [test].mkv"
    );
}

#[test]
fn test_multi_version7() {
    let result = resolve(&[
        "/movies/Iron Man/Iron Man - B (2006).mkv",
        "/movies/Iron Man/Iron Man - C (2007).mkv",
    ]);
    assert_eq!(result.len(), 2);
}

#[test]
fn test_multi_version8() {
    let result = resolve(&[
        "/movies/Iron Man/Iron Man.mkv",
        "/movies/Iron Man/Iron Man_720p.mkv",
        "/movies/Iron Man/Iron Man_test.mkv",
        "/movies/Iron Man/Iron Man_bluray.mkv",
        "/movies/Iron Man/Iron Man_3d.mkv",
        "/movies/Iron Man/Iron Man_3d-hsbs.mkv",
        "/movies/Iron Man/Iron Man_3d.hsbs.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 6);

    let hsbs = result[0]
        .alternate_versions
        .iter()
        .find(|v| v.files[0].path.contains("3d-hsbs"))
        .expect("hsbs alternate present");
    assert!(hsbs.files[0].is_3d);
    assert_eq!(hsbs.files[0].format_3d.as_deref(), Some("hsbs"));
}

#[test]
fn test_multi_version9() {
    let result = resolve(&[
        "/movies/Iron Man/Iron Man (2007).mkv",
        "/movies/Iron Man/Iron Man (2008).mkv",
        "/movies/Iron Man/Iron Man (2009).mkv",
        "/movies/Iron Man/Iron Man (2010).mkv",
        "/movies/Iron Man/Iron Man (2011).mkv",
    ]);
    assert_eq!(result.len(), 5);
    assert!(result[0].alternate_versions.is_empty());
}

#[test]
fn test_multi_version10() {
    let result = resolve(&[
        "/movies/Blade Runner (1982)/Blade Runner (1982) [Final Cut] [1080p HEVC AAC].mkv",
        "/movies/Blade Runner (1982)/Blade Runner (1982) [EE by ADM] [480p HEVC AAC,AAC,AAC].mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 1);
}

#[test]
fn test_multi_version11() {
    let result = resolve(&[
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) [1080p] Blu-ray.x264.DTS.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) [2160p] Blu-ray.x265.AAC.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 1);
}

#[test]
fn test_multi_version12() {
    let result = resolve(&[
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - Theatrical Release.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - Directors Cut.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 2160p.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 720p.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016).mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(
        primary_path(&result[0]),
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016).mkv"
    );
    assert_eq!(result[0].alternate_versions.len(), 5);
    let alts: Vec<&str> = result[0]
        .alternate_versions
        .iter()
        .map(primary_path)
        .collect();
    assert_eq!(
        alts[0],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 2160p.mkv"
    );
    assert_eq!(
        alts[1],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p.mkv"
    );
    assert_eq!(
        alts[2],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 720p.mkv"
    );
    assert_eq!(
        alts[3],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - Directors Cut.mkv"
    );
    assert_eq!(
        alts[4],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - Theatrical Release.mkv"
    );
}

#[test]
fn test_multi_version13() {
    let result = resolve(&[
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - Theatrical Release.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - Directors Cut.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 2160p.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p Directors Cut.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 2160p Remux.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p Theatrical Release.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 720p.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p Remux.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 720p Directors Cut.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p High Bitrate.mkv",
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016).mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(
        primary_path(&result[0]),
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016).mkv"
    );
    assert_eq!(result[0].alternate_versions.len(), 11);
    let alts: Vec<&str> = result[0]
        .alternate_versions
        .iter()
        .map(primary_path)
        .collect();
    assert_eq!(
        alts[0],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 2160p.mkv"
    );
    assert_eq!(
        alts[1],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 2160p Remux.mkv"
    );
    assert_eq!(
        alts[2],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p.mkv"
    );
    assert_eq!(
        alts[3],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p Directors Cut.mkv"
    );
    assert_eq!(
        alts[4],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p High Bitrate.mkv"
    );
    assert_eq!(
        alts[5],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p Remux.mkv"
    );
    assert_eq!(
        alts[6],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 1080p Theatrical Release.mkv"
    );
    assert_eq!(
        alts[7],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 720p.mkv"
    );
    assert_eq!(
        alts[8],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - 720p Directors Cut.mkv"
    );
    assert_eq!(
        alts[9],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - Directors Cut.mkv"
    );
    assert_eq!(
        alts[10],
        "/movies/X-Men Apocalypse (2016)/X-Men Apocalypse (2016) - Theatrical Release.mkv"
    );
}

#[test]
fn resolve_given_folder_name_with_brackets_and_hyphens_groups_based_on_folder_name() {
    let result = resolve(&[
        "/movies/John Wick - Kapitel 3 (2019) [imdbid=tt6146586]/John Wick - Kapitel 3 (2019) [imdbid=tt6146586] - Version 1.mkv",
        "/movies/John Wick - Kapitel 3 (2019) [imdbid=tt6146586]/John Wick - Kapitel 3 (2019) [imdbid=tt6146586] - Version 2.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 1);
}

#[test]
fn resolve_given_unclosed_brackets_does_not_group() {
    let result = resolve(&[
        "/movies/John Wick - Chapter 3 (2019)/John Wick - Chapter 3 (2019) [Version 1].mkv",
        "/movies/John Wick - Chapter 3 (2019)/John Wick - Chapter 3 (2019) [Version 2.mkv",
    ]);
    assert_eq!(result.len(), 2);
}

#[test]
fn test_empty_list() {
    let options = NamingOptions::new();
    let resolver = VideoListResolver::new(&options);
    let result = resolver.resolve_simple(&[]);
    assert!(result.is_empty());
}

#[test]
fn resolve_given_underscore_separator_groups_versions() {
    let result = resolve(&[
        "/movies/Movie (2020)/Movie (2020)_4K.mkv",
        "/movies/Movie (2020)/Movie (2020)_1080p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 1);
}

#[test]
fn resolve_given_dot_separator_groups_versions() {
    let result = resolve(&[
        "/movies/Movie (2020)/Movie (2020).UHD.mkv",
        "/movies/Movie (2020)/Movie (2020).1080p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 1);
}

// Episode multi-version tests

#[test]
fn test_multi_version_episode_in_own_folder() {
    let result = resolve_tv(&[
        "/TV/Dexter/Dexter - S01E01/Dexter - S01E01 - 1080p.mkv",
        "/TV/Dexter/Dexter - S01E01/Dexter - S01E01 - 720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 1);
    assert!(primary_path(&result[0]).contains("1080p"));
    assert!(primary_path(&result[0].alternate_versions[0]).contains("720p"));
}

#[test]
fn test_multi_version_episode_mixed_season_folder() {
    let result = resolve_tv(&[
        "/TV/Dexter/Season 1/Dexter - S01E01 - 1080p.mkv",
        "/TV/Dexter/Season 1/Dexter - S01E01 - 720p.mkv",
        "/TV/Dexter/Season 1/Dexter - S01E02.mkv",
        "/TV/Dexter/Season 1/Dexter - S01E03 - 1080p.mkv",
        "/TV/Dexter/Season 1/Dexter - S01E03 - 720p.mkv",
    ]);
    assert_eq!(result.len(), 3);
    let e01 = result
        .iter()
        .find(|r| primary_path(r).contains("S01E01"))
        .unwrap();
    assert_eq!(e01.alternate_versions.len(), 1);
    assert!(primary_path(e01).contains("1080p"));
    let e02 = result
        .iter()
        .find(|r| primary_path(r).contains("S01E02"))
        .unwrap();
    assert!(e02.alternate_versions.is_empty());
    let e03 = result
        .iter()
        .find(|r| primary_path(r).contains("S01E03"))
        .unwrap();
    assert_eq!(e03.alternate_versions.len(), 1);
}

#[test]
fn test_multi_version_episode_dont_collapse() {
    let result = resolve_tv(&[
        "/TV/Dexter/Season 1/Dexter - S01E01.mkv",
        "/TV/Dexter/Season 1/Dexter - S01E02.mkv",
        "/TV/Dexter/Season 1/Dexter - S01E03.mkv",
        "/TV/Dexter/Season 1/Dexter - S01E04.mkv",
        "/TV/Dexter/Season 1/Dexter - S01E05.mkv",
    ]);
    assert_eq!(result.len(), 5);
    assert!(result.iter().all(|r| r.alternate_versions.is_empty()));
}

#[test]
fn test_multi_version_episode_with_version_suffix() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - Aired.mkv",
        "/TV/Show/Season 1/Show - S01E01 - Uncensored.mkv",
        "/TV/Show/Season 1/Show - S01E02 - Aired.mkv",
        "/TV/Show/Season 1/Show - S01E02 - Uncensored.mkv",
    ]);
    assert_eq!(result.len(), 2);
    assert!(result.iter().all(|r| r.alternate_versions.len() == 1));
}

#[test]
fn test_multi_version_episode_four_versions() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - VersionA.mkv",
        "/TV/Show/Season 1/Show - S01E01 - VersionB.mkv",
        "/TV/Show/Season 1/Show - S01E01 - VersionC.mkv",
        "/TV/Show/Season 1/Show - S01E01 - VersionD.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 3);
}

#[test]
fn test_multi_version_episode_with_resolutions() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - 720p.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 2160p.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 1080p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 2);
    assert!(primary_path(&result[0]).contains("2160p"));
    assert!(primary_path(&result[0].alternate_versions[0]).contains("1080p"));
    assert!(primary_path(&result[0].alternate_versions[1]).contains("720p"));
}

#[test]
fn test_multi_version_episode_different_seasons() {
    let result = resolve_tv(&["/TV/Show/Show - S01E01.mkv", "/TV/Show/Show - S02E01.mkv"]);
    assert_eq!(result.len(), 2);
    assert!(result.iter().all(|r| r.alternate_versions.is_empty()));
}

#[test]
fn test_multi_version_episode_disabled_by_default() {
    let result = resolve(&[
        "/TV/Show/Season 1/Show - S01E01 - 1080p.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 720p.mkv",
    ]);
    assert_eq!(result.len(), 2);
}

#[test]
fn test_multi_version_episode_same_number_different_title() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - Pilot.mkv",
        "/TV/Show/Season 1/Show - S01E01 - Completely Different Title.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 1);
}

#[test]
fn test_multi_version_episode_with_title() {
    let result = resolve_tv(&[
        "/TV/Show/Show - S01E01/Show - S01E01 - Episode Title - 1080p.mkv",
        "/TV/Show/Show - S01E01/Show - S01E01 - Episode Title - 720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 1);
    assert!(primary_path(&result[0]).contains("1080p"));
    assert!(primary_path(&result[0].alternate_versions[0]).contains("720p"));
}

#[test]
fn test_multi_version_episode_with_title_mixed_folder() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - Pilot - 1080p.mkv",
        "/TV/Show/Season 1/Show - S01E01 - Pilot - 720p.mkv",
        "/TV/Show/Season 1/Show - S01E02 - Second Episode - 1080p.mkv",
        "/TV/Show/Season 1/Show - S01E02 - Second Episode - 720p.mkv",
        "/TV/Show/Season 1/Show - S01E03 - Third Episode.mkv",
    ]);
    assert_eq!(result.len(), 3);
    let e01 = result
        .iter()
        .find(|r| primary_path(r).contains("S01E01"))
        .unwrap();
    assert_eq!(e01.alternate_versions.len(), 1);
    let e02 = result
        .iter()
        .find(|r| primary_path(r).contains("S01E02"))
        .unwrap();
    assert_eq!(e02.alternate_versions.len(), 1);
    let e03 = result
        .iter()
        .find(|r| primary_path(r).contains("S01E03"))
        .unwrap();
    assert!(e03.alternate_versions.is_empty());
}

#[test]
fn test_multi_version_episode_in_season_subfolder() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01/Show - S01E01 - 1080p.mkv",
        "/TV/Show/Season 1/Show - S01E01/Show - S01E01 - 720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].alternate_versions.len(), 1);
    assert!(primary_path(&result[0]).contains("1080p"));
    assert!(primary_path(&result[0].alternate_versions[0]).contains("720p"));
}

#[test]
fn test_multi_version_episode_with_title_and_version_suffix() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - Pilot - Aired.mkv",
        "/TV/Show/Season 1/Show - S01E01 - Pilot - Uncensored.mkv",
        "/TV/Show/Season 1/Show - S01E02 - The Getaway - Aired.mkv",
        "/TV/Show/Season 1/Show - S01E02 - The Getaway - Uncensored.mkv",
    ]);
    assert_eq!(result.len(), 2);
    assert!(result.iter().all(|r| r.alternate_versions.len() == 1));
}

#[test]
fn test_multi_version_episode_with_additional_parts_cd() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - 1080p cd1.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 1080p cd2.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
    assert_eq!(result[0].alternate_versions.len(), 1);
    assert!(primary_path(&result[0].alternate_versions[0]).contains("720p"));
}

#[test]
fn test_multi_version_episode_with_additional_parts_dash_part() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - 1080p - part1.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 1080p - part2.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
    assert_eq!(result[0].alternate_versions.len(), 1);
    assert!(primary_path(&result[0].alternate_versions[0]).contains("720p"));
}

#[test]
fn test_multi_version_episode_with_additional_parts_pt() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - 1080p.pt1.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 1080p.pt2.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
    assert_eq!(result[0].alternate_versions.len(), 1);
    assert!(primary_path(&result[0].alternate_versions[0]).contains("720p"));
}

#[test]
fn test_multi_version_episode_with_additional_parts_and_title() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - Pilot - 1080p part1.mkv",
        "/TV/Show/Season 1/Show - S01E01 - Pilot - 1080p part2.mkv",
        "/TV/Show/Season 1/Show - S01E01 - Pilot - 720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
    assert_eq!(result[0].alternate_versions.len(), 1);
    assert!(primary_path(&result[0].alternate_versions[0]).contains("720p"));
}

#[test]
fn test_multi_version_episode_with_additional_parts_and_title_dash_separator() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - Pilot - 1080p - part1.mkv",
        "/TV/Show/Season 1/Show - S01E01 - Pilot - 1080p - part2.mkv",
        "/TV/Show/Season 1/Show - S01E01 - Pilot - 720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
    assert_eq!(result[0].alternate_versions.len(), 1);
    assert!(primary_path(&result[0].alternate_versions[0]).contains("720p"));
}

#[test]
fn test_multi_version_episode_with_additional_parts_and_multiple_episodes() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - 1080p cd1.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 1080p cd2.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 720p.mkv",
        "/TV/Show/Season 1/Show - S01E02 - Other.mkv",
    ]);
    assert_eq!(result.len(), 2);
    let e01 = result
        .iter()
        .find(|r| primary_path(r).contains("S01E01"))
        .unwrap();
    assert_eq!(e01.files.len(), 2);
    assert_eq!(e01.alternate_versions.len(), 1);
    let e02 = result
        .iter()
        .find(|r| primary_path(r).contains("S01E02"))
        .unwrap();
    assert!(e02.alternate_versions.is_empty());
}

#[test]
fn test_multi_version_episode_part_stack_alongside_single_file_resolutions() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/S01E01 - 720p.mkv",
        "/TV/Show/Season 1/S01E01 - 1080p.mkv",
        "/TV/Show/Season 1/S01E01 - Part 1.mkv",
        "/TV/Show/Season 1/S01E01 - Part 2.mkv",
        "/TV/Show/Season 1/S01E01 - Part 3.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 3);
    assert!(result[0].files.iter().all(|f| f.path.contains("Part")));
    assert_eq!(result[0].alternate_versions.len(), 2);
    assert!(
        result[0]
            .alternate_versions
            .iter()
            .any(|f| f.files[0].path.contains("1080p"))
    );
    assert!(
        result[0]
            .alternate_versions
            .iter()
            .any(|f| f.files[0].path.contains("720p"))
    );
}

#[test]
fn test_multi_version_episode_two_part_stacks() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - 1080p - part1.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 1080p - part2.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 720p - part1.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 720p - part2.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
    assert!(primary_path(&result[0]).contains("1080p"));
    assert_eq!(result[0].alternate_versions.len(), 1);
    let alt = &result[0].alternate_versions[0];
    assert_eq!(alt.files.len(), 2);
    assert!(alt.files.iter().all(|f| f.path.contains("720p")));
}

#[test]
fn test_multi_version_episode_part_stack_with_trailer() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - 1080p part1.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 1080p part2.mkv",
        "/TV/Show/Season 1/Show - S01E01 - 720p.mkv",
        "/TV/Show/Season 1/Show - S01E01-trailer.mp4",
    ]);
    assert_eq!(result.len(), 2);
    let episode = result.iter().find(|r| r.extra_type.is_none()).unwrap();
    assert_eq!(episode.files.len(), 2);
    assert_eq!(episode.alternate_versions.len(), 1);
    assert!(primary_path(&episode.alternate_versions[0]).contains("720p"));
    let trailer = result.iter().find(|r| r.extra_type.is_some()).unwrap();
    assert_eq!(trailer.extra_type, Some(ExtraType::Trailer));
}

#[test]
fn test_movie_stacking_with_part_naming() {
    let result = resolve(&[
        "/movies/Movie/Movie part1.mkv",
        "/movies/Movie/Movie part2.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
}

#[test]
fn test_movie_stacking_with_dash_part_naming() {
    let result = resolve(&[
        "/movies/Movie/Movie - part1.mkv",
        "/movies/Movie/Movie - part2.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
}

#[test]
fn test_movie_stacking_with_pt_naming() {
    let result = resolve(&["/movies/Movie/Movie.pt1.mkv", "/movies/Movie/Movie.pt2.mkv"]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
}

#[test]
fn test_movie_stacking_with_hyphen_no_spaces() {
    let result = resolve(&[
        "/movies/Movie/Movie-part1.mkv",
        "/movies/Movie/Movie-part2.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
}

#[test]
fn test_movie_stacking_with_hyphen_no_spaces_and_version() {
    let result = resolve(&[
        "/movies/Movie/Movie-1080p-part1.mkv",
        "/movies/Movie/Movie-1080p-part2.mkv",
        "/movies/Movie/Movie-720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
    assert_eq!(result[0].alternate_versions.len(), 1);
}

#[test]
fn test_movie_multi_version_with_stacked_alternate() {
    let result = resolve(&[
        "/movies/Inception (2010)/Inception (2010).mkv",
        "/movies/Inception (2010)/Inception (2010) - 4k part1.mkv",
        "/movies/Inception (2010)/Inception (2010) - 4k part2.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 1);
    assert_eq!(
        primary_path(&result[0]),
        "/movies/Inception (2010)/Inception (2010).mkv"
    );
    assert_eq!(result[0].alternate_versions.len(), 1);
    let stacked = &result[0].alternate_versions[0];
    assert_eq!(stacked.files.len(), 2);
    assert!(stacked.files.iter().all(|f| f.path.contains("4k part")));
}

#[test]
fn test_episode_stacking_with_hyphen_no_spaces() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01-1080p-cd1.mkv",
        "/TV/Show/Season 1/Show - S01E01-1080p-cd2.mkv",
        "/TV/Show/Season 1/Show - S01E01-720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
    assert_eq!(result[0].alternate_versions.len(), 1);
}

#[test]
fn test_episode_stacking_with_hyphen_no_spaces_and_title() {
    let result = resolve_tv(&[
        "/TV/Show/Season 1/Show - S01E01 - Pilot-1080p-part1.mkv",
        "/TV/Show/Season 1/Show - S01E01 - Pilot-1080p-part2.mkv",
        "/TV/Show/Season 1/Show - S01E01 - Pilot-720p.mkv",
    ]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].files.len(), 2);
    assert_eq!(result[0].alternate_versions.len(), 1);
}
