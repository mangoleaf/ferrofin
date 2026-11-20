//! The NFO and image sidecars a DVR recording gets when its capture starts —
//! port of `RecordingsMetadataManager` (`src/Jellyfin.LiveTv/Recordings/`).
//!
//! The programme facts come from the timer's [`TimerRecordingInfo`], which
//! the guide row filled when the timer fired (upstream's programme lookup by
//! `ProgramId`, falling back to the timer's own fields: its "dummy"). The
//! guide stores no community or critic rating, people, provider ids, studios,
//! tagline or original title, so the elements upstream writes from those are
//! never written here.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ferrofin_model::entities::ImageType;
use ferrofin_traits::options::ItemImageInfo;

use crate::dvr::TimerRecordingInfo;

/// `RecordingsMetadataManager.DateAddedFormat`, written in UTC since Jellyfin
/// 12.0 (the NFO parser reads it as UTC).
const DATE_ADDED_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// Writes a date in the NFO options' `ReleaseDateFormat`, as
/// `DateTime.ToString(format, CultureInfo.InvariantCulture)` does.
pub type ReleaseDateFormat = Arc<dyn Fn(DateTime<Utc>) -> String + Send + Sync>;

/// The `ReleaseDateFormat` of default NFO options, `yyyy-MM-dd`.
#[must_use]
pub fn default_release_date_format() -> ReleaseDateFormat {
    Arc::new(|date| date.format("%Y-%m-%d").to_string())
}

/// `SaveRecordingMetadata`'s NFO half, run with "Save EPG metadata to NFO
/// files" (`LiveTvOptions.SaveRecordingNFO`): a series episode gets its
/// series folder's `tvshow.nfo` and its own `<episodedetails>`; anything
/// else a `<movie>`, locked unless it is a movie that is neither sports nor
/// news. The programme's genres gain Sports, Kids and Children, or News from
/// the timer's flags. An NFO that exists already is kept.
///
/// # Errors
///
/// The I/O error that stopped a write, or a series episode without a series
/// folder (upstream's `ArgumentNullException`). The caller logs it: a sidecar
/// never fails a recording.
pub async fn save_recording_nfo(
    info: &TimerRecordingInfo,
    recording: &Path,
    series: Option<&Path>,
    release_date: &ReleaseDateFormat,
    now: DateTime<Utc>,
) -> std::io::Result<()> {
    if let Some(folder) = recording.parent() {
        tokio::fs::create_dir_all(folder).await?;
    }
    let mut genres = info.genres.clone();
    if info.is_sports {
        add_genre(&mut genres, "Sports");
    }
    if info.is_kids {
        add_genre(&mut genres, "Kids");
        add_genre(&mut genres, "Children");
    }
    if info.is_news {
        add_genre(&mut genres, "News");
    }
    let video = VideoNfo {
        info,
        genres: &genres,
        release_date,
        now,
    };
    if info.is_program_series {
        let series = series.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a series recording has no series folder",
            )
        })?;
        create_new(&series.join("tvshow.nfo"), &series_nfo(info)).await?;
        create_new(&recording.with_extension("nfo"), &video.render(false)).await
    } else {
        let lock_data = !info.is_movie || info.is_sports || info.is_news;
        create_new(&recording.with_extension("nfo"), &video.render(lock_data)).await
    }
}

/// `BaseItem.AddGenre`: adds `genre` unless the list has it, ignoring case.
fn add_genre(genres: &mut Vec<String>, genre: &str) {
    if !genres.iter().any(|g| g.eq_ignore_ascii_case(genre)) {
        genres.push(genre.to_owned());
    }
}

/// Writes `body` to a new file at `path`, leaving one that exists alone
/// (`File.Exists` → return, then `FileMode.CreateNew`).
async fn create_new(path: &Path, body: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e),
    };
    file.write_all(body.as_bytes()).await?;
    file.flush().await
}

/// `SaveRecordingImages`' choice, run with "Save EPG images"
/// (`LiveTvOptions.SaveRecordingImages`): the programme's images to copy
/// beside `recording`, in its order, each with the path it is saved at before
/// its extension. A series episode gets its Thumb, else its Primary, as
/// `{recording}-thumb`; anything else its Primary (`poster`), else its Thumb,
/// then its Backdrop (`fanart`), its Thumb (`landscape`) and its Logo
/// (`logo`). The name follows the image chosen, so a Thumb standing in for a
/// Primary is a `landscape`, and upstream copies it twice.
#[must_use]
pub fn recording_images<'a>(
    recording: &Path,
    is_series: bool,
    images: &'a [ItemImageInfo],
) -> Vec<(&'a ItemImageInfo, PathBuf)> {
    let first = |image_type| images.iter().find(|i| i.image_type == image_type);
    let chosen = if is_series {
        vec![first(ImageType::Thumb).or_else(|| first(ImageType::Primary))]
    } else {
        vec![
            first(ImageType::Primary).or_else(|| first(ImageType::Thumb)),
            first(ImageType::Backdrop),
            first(ImageType::Thumb),
            first(ImageType::Logo),
        ]
    };
    let stem = recording.file_stem().unwrap_or_default().to_string_lossy();
    chosen
        .into_iter()
        .flatten()
        .filter_map(|image| {
            let name = match image.image_type {
                ImageType::Primary | ImageType::Thumb if is_series => format!("{stem}-thumb"),
                ImageType::Primary => "poster".to_owned(),
                ImageType::Thumb => "landscape".to_owned(),
                ImageType::Backdrop => "fanart".to_owned(),
                ImageType::Logo => "logo".to_owned(),
                _ => return None,
            };
            Some((image, recording.with_file_name(name)))
        })
        .collect()
}

/// `SaveRecordingImage`'s copy: the local image `local` copied to `name` with
/// `local`'s extension, replacing a file there (`File.Copy(.., true)`).
///
/// Upstream sets the extension with `Path.ChangeExtension`, which cuts the
/// name at its last dot, so an episode `Dr. Who S01E01` saved its thumb as
/// `Dr.jpg`; the extension is appended here.
///
/// # Errors
///
/// The I/O error that stopped the copy. The caller logs it and goes on to the
/// next image, as upstream's catch does.
pub async fn copy_recording_image(local: &Path, name: &Path) -> std::io::Result<PathBuf> {
    let mut target = name.as_os_str().to_owned();
    if let Some(extension) = local.extension() {
        target.push(".");
        target.push(extension);
    }
    let target = PathBuf::from(target);
    if let Some(folder) = target.parent() {
        tokio::fs::create_dir_all(folder).await?;
    }
    tokio::fs::copy(local, &target).await?;
    Ok(target)
}

/// `SaveSeriesNfoAsync`: the series folder's `tvshow.nfo`, from the timer.
/// Its `id`/`imdb_id`/`tmdbid`/`zap2itid` come from the timer's series
/// provider ids, which Ferrofin's guide does not store.
fn series_nfo(info: &TimerRecordingInfo) -> String {
    let mut nfo = Nfo::new("tvshow");
    if !info.name.trim().is_empty() {
        nfo.element("title", &info.name);
    }
    if let Some(rating) = info
        .official_rating
        .as_deref()
        .filter(|r| !r.trim().is_empty())
    {
        nfo.element("mpaa", rating);
    }
    for genre in &info.genres {
        nfo.element("genre", genre);
    }
    nfo.finish("tvshow")
}

/// `SaveVideoNfoAsync`'s inputs: the programme (the timer's guide facts),
/// its genres with the flags', the release-date format and the clock.
struct VideoNfo<'a> {
    info: &'a TimerRecordingInfo,
    genres: &'a [String],
    release_date: &'a ReleaseDateFormat,
    now: DateTime<Utc>,
}

impl VideoNfo<'_> {
    /// The recording's own NFO, in upstream's element order.
    fn render(&self, lock_data: bool) -> String {
        let info = self.info;
        let root = if info.is_program_series {
            "episodedetails"
        } else {
            "movie"
        };
        let mut nfo = Nfo::new(root);
        if info.is_program_series {
            if let Some(title) = info
                .episode_title
                .as_deref()
                .filter(|t| !t.trim().is_empty())
            {
                nfo.element("title", title);
            }
            // A first airing with no air date is dated now.
            let aired = info
                .original_air_date
                .or_else(|| (!info.is_repeat).then_some(self.now));
            if let Some(aired) = aired {
                nfo.element("aired", &(self.release_date)(aired));
            }
            if let Some(episode) = info.episode_number {
                nfo.element("episode", &episode.to_string());
            }
            if let Some(season) = info.season_number {
                nfo.element("season", &season.to_string());
            }
        } else {
            if !info.name.trim().is_empty() {
                nfo.element("title", &info.name);
            }
            if let Some(premiered) = info.original_air_date {
                let date = (self.release_date)(premiered);
                nfo.element("premiered", &date);
                nfo.element("releasedate", &date);
            }
        }
        nfo.element("dateadded", &self.now.format(DATE_ADDED_FORMAT).to_string());
        if let Some(year) = info.production_year {
            nfo.element("year", &year.to_string());
        }
        if let Some(rating) = info.official_rating.as_deref().filter(|r| !r.is_empty()) {
            nfo.element("mpaa", rating);
        }
        let plot =
            ferrofin_common::extensions::strip_html(info.overview.as_deref().unwrap_or_default())
                .replace("&quot;", "'");
        nfo.element("plot", &plot);
        for genre in self.genres {
            nfo.element("genre", genre);
        }
        if lock_data {
            nfo.element("lockdata", "true");
        }
        nfo.finish(root)
    }
}

/// The subset of `System.Xml.XmlWriter` these NFOs use: the standalone UTF-8
/// prolog of `WriteStartDocument(true)`, one root element, and two-space
/// indented leaf elements (`WriteElementString`), an empty one as
/// `<name />`.
struct Nfo {
    buf: String,
}

impl Nfo {
    /// Starts the document and opens `root`.
    fn new(root: &str) -> Self {
        Self {
            buf: format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<{root}>\n"
            ),
        }
    }

    /// Writes `<name>value</name>`, escaping `&`, `<` and `>`.
    ///
    /// Guide text can carry characters XML 1.0 cannot hold; `XmlWriter`
    /// throws on those, which upstream's catch-all turns into a half-written
    /// NFO kept forever. They are dropped instead, and line breaks follow
    /// `NewLineHandling.Replace` (`\r\n` and a lone `\r` become `\n`).
    fn element(&mut self, name: &str, value: &str) {
        let value: String = value
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|&c| is_xml_char(c))
            .collect();
        if value.is_empty() {
            let _ = writeln!(self.buf, "  <{name} />");
        } else {
            let value = value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let _ = writeln!(self.buf, "  <{name}>{value}</{name}>");
        }
    }

    /// Closes `root` and returns the document.
    fn finish(mut self, root: &str) -> String {
        let _ = write!(self.buf, "</{root}>");
        self.buf
    }
}

/// Whether `c` may appear in an XML 1.0 document (the `Char` production).
fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 21, 5, 9)
            .single()
            .expect("date")
    }

    async fn save(
        info: &TimerRecordingInfo,
        dir: &Path,
        series: Option<&Path>,
    ) -> std::io::Result<std::path::PathBuf> {
        let recording = dir.join("Show S01E02.ts");
        save_recording_nfo(
            info,
            &recording,
            series,
            &default_release_date_format(),
            now(),
        )
        .await?;
        Ok(recording.with_extension("nfo"))
    }

    /// Upstream `SaveRecordingMetadata_DateAddedIsUtc`: `dateadded` is the
    /// UTC instant, in `yyyy-MM-dd HH:mm:ss`; a timer with only a name gets a
    /// locked `<movie>` with an empty plot.
    #[tokio::test]
    async fn date_added_is_utc() {
        let dir = tempfile::tempdir().expect("tmp");
        let info = TimerRecordingInfo {
            name: "Test Recording".to_owned(),
            ..TimerRecordingInfo::default()
        };
        let nfo = save(&info, dir.path(), None).await.expect("saved");
        assert_eq!(
            tokio::fs::read_to_string(nfo).await.expect("nfo"),
            "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<movie>\n  \
             <title>Test Recording</title>\n  <dateadded>2026-10-20 21:05:09</dateadded>\n  \
             <plot />\n  <lockdata>true</lockdata>\n</movie>"
        );
    }

    /// A series episode: the series folder's `tvshow.nfo` from the timer,
    /// and the episode's own, with the flags' genres added once.
    #[tokio::test]
    async fn a_series_episode_gets_its_show_and_episode_nfos() {
        let dir = tempfile::tempdir().expect("tmp");
        let series = dir.path().join("Show");
        let season = series.join("Season 1");
        let info = TimerRecordingInfo {
            name: "Show".to_owned(),
            episode_title: Some("Pilot & <Part 1>".to_owned()),
            overview: Some("<p>A &quot;new&quot; start.</p>".to_owned()),
            genres: vec!["Kids".to_owned(), "Comedy".to_owned()],
            official_rating: Some("TV-PG".to_owned()),
            season_number: Some(1),
            episode_number: Some(2),
            production_year: Some(2026),
            is_program_series: true,
            is_kids: true,
            ..TimerRecordingInfo::default()
        };
        let nfo = save(&info, &season, Some(&series)).await.expect("saved");
        assert_eq!(
            tokio::fs::read_to_string(series.join("tvshow.nfo"))
                .await
                .expect("tvshow"),
            "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<tvshow>\n  \
             <title>Show</title>\n  <mpaa>TV-PG</mpaa>\n  <genre>Kids</genre>\n  \
             <genre>Comedy</genre>\n</tvshow>"
        );
        assert_eq!(
            tokio::fs::read_to_string(nfo).await.expect("episode"),
            "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<episodedetails>\n  \
             <title>Pilot &amp; &lt;Part 1&gt;</title>\n  <aired>2026-10-20</aired>\n  \
             <episode>2</episode>\n  <season>1</season>\n  \
             <dateadded>2026-10-20 21:05:09</dateadded>\n  <year>2026</year>\n  \
             <mpaa>TV-PG</mpaa>\n  <plot>A 'new' start.</plot>\n  <genre>Kids</genre>\n  \
             <genre>Comedy</genre>\n  <genre>Children</genre>\n</episodedetails>"
        );
    }

    /// Guide text with characters XML cannot hold still makes a well-formed
    /// NFO: they are dropped, and a carriage return becomes a newline.
    #[tokio::test]
    async fn characters_xml_cannot_hold_are_dropped() {
        let dir = tempfile::tempdir().expect("tmp");
        let info = TimerRecordingInfo {
            name: "Bell\u{7} Show".to_owned(),
            overview: Some("one\r\ntwo\rthree\u{0}".to_owned()),
            ..TimerRecordingInfo::default()
        };
        let nfo = save(&info, dir.path(), None).await.expect("saved");
        let body = tokio::fs::read_to_string(nfo).await.expect("nfo");
        assert!(body.contains("  <title>Bell Show</title>\n"), "{body}");
        assert!(body.contains("  <plot>one\ntwo\nthree</plot>\n"), "{body}");
    }

    /// A repeat with no air date is not dated; a series recording with no
    /// series folder saves nothing (upstream's `ArgumentNullException`).
    #[tokio::test]
    async fn a_repeat_is_undated_and_a_series_needs_its_folder() {
        let dir = tempfile::tempdir().expect("tmp");
        let info = TimerRecordingInfo {
            name: "Show".to_owned(),
            is_program_series: true,
            is_repeat: true,
            ..TimerRecordingInfo::default()
        };
        let nfo = save(&info, dir.path(), Some(dir.path()))
            .await
            .expect("saved");
        let body = tokio::fs::read_to_string(&nfo).await.expect("nfo");
        assert!(!body.contains("<aired>"), "{body}");
        assert!(
            save(&info, &dir.path().join("other"), None).await.is_err(),
            "no series folder"
        );
        assert!(!dir.path().join("other").join("Show S01E02.nfo").exists());
    }

    /// A movie is unlocked unless it is sports or news, and dated with its
    /// air date in the NFO options' format; an NFO already there is kept.
    #[tokio::test]
    async fn a_movie_is_locked_only_as_sports_or_news_and_an_nfo_is_kept() {
        let dir = tempfile::tempdir().expect("tmp");
        let movie = TimerRecordingInfo {
            name: "Film".to_owned(),
            is_movie: true,
            original_air_date: Some(
                Utc.with_ymd_and_hms(1999, 3, 31, 0, 0, 0)
                    .single()
                    .expect("date"),
            ),
            ..TimerRecordingInfo::default()
        };
        let release_date: ReleaseDateFormat = Arc::new(|d| d.format("%d.%m.%Y").to_string());
        let recording = dir.path().join("Film.ts");
        save_recording_nfo(&movie, &recording, None, &release_date, now())
            .await
            .expect("saved");
        let nfo = recording.with_extension("nfo");
        let body = tokio::fs::read_to_string(&nfo).await.expect("nfo");
        assert!(
            body.contains(
                "  <premiered>31.03.1999</premiered>\n  <releasedate>31.03.1999</releasedate>\n"
            ),
            "{body}"
        );
        assert!(!body.contains("<lockdata>"), "{body}");

        let sports = TimerRecordingInfo {
            is_sports: true,
            ..movie.clone()
        };
        tokio::fs::remove_file(&nfo).await.expect("rm");
        save_recording_nfo(&sports, &recording, None, &release_date, now())
            .await
            .expect("saved");
        let body = tokio::fs::read_to_string(&nfo).await.expect("nfo");
        assert!(
            body.contains("<genre>Sports</genre>\n  <lockdata>true</lockdata>"),
            "{body}"
        );

        tokio::fs::write(&nfo, "mine").await.expect("write");
        save_recording_nfo(&movie, &recording, None, &release_date, now())
            .await
            .expect("saved");
        assert_eq!(tokio::fs::read_to_string(&nfo).await.expect("nfo"), "mine");
    }

    /// `SaveRecordingImages`' choice, each image named by its own type: a
    /// series episode's Thumb, else its Primary; anything else's Primary,
    /// else its Thumb, then its Backdrop, Thumb and Logo.
    #[rstest::rstest]
    #[case::series_thumb_first(true, &[ImageType::Primary, ImageType::Thumb], &[(ImageType::Thumb, "Show S01E02-thumb")])]
    #[case::series_primary(true, &[ImageType::Backdrop, ImageType::Primary], &[(ImageType::Primary, "Show S01E02-thumb")])]
    #[case::series_nothing_else(true, &[ImageType::Backdrop, ImageType::Logo], &[])]
    #[case::all_four(
        false,
        &[ImageType::Logo, ImageType::Thumb, ImageType::Backdrop, ImageType::Primary],
        &[(ImageType::Primary, "poster"), (ImageType::Backdrop, "fanart"), (ImageType::Thumb, "landscape"), (ImageType::Logo, "logo")]
    )]
    #[case::thumb_for_primary(false, &[ImageType::Thumb], &[(ImageType::Thumb, "landscape"), (ImageType::Thumb, "landscape")])]
    #[case::no_images(false, &[], &[])]
    fn recording_images_follow_upstreams_choice(
        #[case] is_series: bool,
        #[case] types: &[ImageType],
        #[case] expected: &[(ImageType, &str)],
    ) {
        let folder = Path::new("/recordings/Show");
        let images: Vec<ItemImageInfo> = types
            .iter()
            .map(|&image_type| ItemImageInfo {
                image_type,
                path: format!("http://img/{image_type:?}"),
                ..ItemImageInfo::default()
            })
            .collect();
        let chosen: Vec<(ImageType, PathBuf)> =
            recording_images(&folder.join("Show S01E02.ts"), is_series, &images)
                .into_iter()
                .map(|(image, name)| (image.image_type, name))
                .collect();
        let expected: Vec<(ImageType, PathBuf)> = expected
            .iter()
            .map(|&(image_type, name)| (image_type, folder.join(name)))
            .collect();
        assert_eq!(chosen, expected);
    }

    /// `File.Copy(.., true)` with the image's extension: a file already there
    /// is replaced, and an image without an extension gives a name without one.
    #[tokio::test]
    async fn an_image_is_copied_with_its_extension_over_one_there() {
        let dir = tempfile::tempdir().expect("tmp");
        let local = dir.path().join("image.png");
        tokio::fs::write(&local, "new").await.expect("image");
        let folder = dir.path().join("Film");
        tokio::fs::create_dir_all(&folder).await.expect("folder");
        tokio::fs::write(folder.join("poster.png"), "old")
            .await
            .expect("old");

        let saved = copy_recording_image(&local, &folder.join("poster"))
            .await
            .expect("copied");
        assert_eq!(saved, folder.join("poster.png"));
        assert_eq!(
            tokio::fs::read_to_string(&saved).await.expect("read"),
            "new"
        );

        let bare = dir.path().join("image");
        tokio::fs::write(&bare, "bare").await.expect("image");
        let saved = copy_recording_image(&bare, &dir.path().join("New").join("logo"))
            .await
            .expect("copied");
        assert_eq!(saved, dir.path().join("New").join("logo"));
    }
}
