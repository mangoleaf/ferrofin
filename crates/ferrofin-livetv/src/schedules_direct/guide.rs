//! The guide half of the Schedules Direct provider: `GetProgramsAsync`,
//! `GetImageForPrograms` and `GetProgram` (v12.2
//! `Jellyfin.LiveTv/Listings/SchedulesDirect.cs`).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, Offset, TimeZone, Utc};

use ferrofin_model::live_tv::{ListingsProviderInfo, ProgramAudio};
use ferrofin_traits::error::ServiceError;

use super::dto::{ImageDataDto, ProgramDetailsDto, ProgramDto, RequestScheduleForChannelDto};
use super::{API_URL, SchedulesDirect, SdError, SdErrorCode, SdRequest, to_json};
use crate::fetch::RequestMethod;
use crate::mapping::eq_ordinal_ignore_case;

/// The most program ids one `/metadata/programs/` request may carry (SD API).
const IMAGE_BATCH_SIZE: usize = 500;

/// `GuideManager.MaxCacheDays`: artwork is attached only to airings ending
/// within this many days, the ones the guide pre-caches while the token in
/// their URLs is still valid.
const MAX_CACHE_DAYS: i64 = 2;

/// `ProgramInfo`, as `SchedulesDirect.GetProgram` fills it.
///
/// Its `Debug` names the airing but leaves out the image URLs, which carry
/// the SD session token.
#[derive(Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // they are the upstream ProgramInfo flags
pub struct SdProgram {
    /// `Id`: `{programID}T{start ticks}C{station}`. `ListingsManager` appends
    /// `_{channel id}` per tuner channel.
    pub id: String,
    /// `ChannelId`: the station id.
    pub channel_id: String,
    /// `StartDate`.
    pub start: DateTime<Utc>,
    /// `EndDate`: start plus the airing's duration.
    pub end: DateTime<Utc>,
    /// `Name`: the first title, or `"Unknown"`.
    pub name: String,
    /// `EpisodeTitle`: `episodeTitle150`.
    pub episode_title: Option<String>,
    /// `Overview`: the first 1000-character description, else the first
    /// 100-character one.
    pub overview: Option<String>,
    /// `Genres`, blanks dropped.
    pub genres: Vec<String>,
    /// `OfficialRating`: the first content rating, `TV` codes hyphenated.
    pub official_rating: Option<String>,
    /// `Audio`.
    pub audio: ProgramAudio,
    /// `ImageUrl`: the best 2:3 artwork.
    pub image_url: Option<String>,
    /// `ThumbImageUrl`: the best 16:9 artwork with text, when it is not the
    /// primary image.
    pub thumb_image_url: Option<String>,
    /// `IsRepeat`: the airing is not marked `new`.
    pub is_repeat: bool,
    /// `IsSeries`.
    pub is_series: bool,
    /// `IsKids`.
    pub is_kids: bool,
    /// `IsSports`.
    pub is_sports: bool,
    /// `IsMovie`.
    pub is_movie: bool,
    /// `IsNews`.
    pub is_news: bool,
    /// `IsLive`.
    pub is_live: bool,
    /// `IsPremiere`.
    pub is_premiere: bool,
    /// `IsHD`.
    pub is_hd: bool,
    /// `Is3D`.
    pub is_3d: bool,
    /// `Etag`: the airing's `md5`.
    pub etag: Option<String>,
    /// `ShowId`.
    pub show_id: String,
    /// `SeriesId` (also its `Zap2It` series provider id): the first ten
    /// characters of the program id.
    pub series_id: Option<String>,
    /// `SeasonNumber`.
    pub season_number: Option<i32>,
    /// `EpisodeNumber`.
    pub episode_number: Option<i32>,
    /// `OriginalAirDate`.
    pub original_air_date: Option<DateTime<Utc>>,
    /// `ProductionYear`.
    pub production_year: Option<i32>,
}

impl std::fmt::Debug for SdProgram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SdProgram")
            .field("id", &self.id)
            .field("channel_id", &self.channel_id)
            .field("start", &self.start)
            .field("end", &self.end)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl SchedulesDirect {
    /// One station's airings over `[start, end]`: port of `GetProgramsAsync`.
    ///
    /// `Ok(None)` is "SD gave no guide data": the metadata daily limit is in
    /// force, or SD gave no token (see [`SchedulesDirect::get_channels`]).
    /// Upstream answers `[]` there, which its guide refresh then reads as a
    /// station without airings and drains; the caller here keeps the stored
    /// guide instead.
    ///
    /// # Errors
    ///
    /// A blank `channel_id` is [`ServiceError::InvalidInput`]; a failed SD
    /// request, or an airing whose program SD sent no details for, is a
    /// backend error.
    pub async fn get_programs(
        &self,
        info: &ListingsProviderInfo,
        channel_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Option<Vec<SdProgram>>, ServiceError> {
        if self.is_metadata_limit_active() {
            return Ok(None);
        }
        if channel_id.is_empty() {
            return Err(SdError::Argument("channelId cannot be null or empty".to_owned()).into());
        }
        let channel_id = normalize_station_id(channel_id);

        let Some(token) = self.get_token(info).await?.filter(|t| !t.is_empty()) else {
            return Ok(None);
        };

        let dates = schedule_request_dates(start, end, |at| {
            Local.offset_from_utc_datetime(&at.naive_utc()).fix()
        });
        tracing::debug!(station = %channel_id, "requesting the station's schedules from Schedules Direct");
        let body = to_json(&[RequestScheduleForChannelDto {
            station_id: channel_id.clone(),
            date: dates,
        }])?;
        let Some(days) = self
            .request::<Vec<super::dto::DayDto>>(
                SdRequest::new(RequestMethod::Post, "/schedules", Some(&token)).json(body),
                true,
                Some(info),
            )
            .await?
        else {
            return Ok(Some(Vec::new()));
        };

        // `.Distinct()`: first-seen order.
        let mut seen = HashSet::new();
        let program_ids: Vec<&str> = days
            .iter()
            .flat_map(|day| &day.programs)
            .filter_map(|airing| airing.program_id.as_deref())
            .filter(|id| seen.insert(*id))
            .collect();
        let Some(details) = self
            .request::<Vec<ProgramDetailsDto>>(
                SdRequest::new(RequestMethod::Post, "/programs", Some(&token))
                    .json(to_json(&program_ids)?),
                true,
                Some(info),
            )
            .await?
        else {
            return Ok(Some(Vec::new()));
        };

        let mut by_id: HashMap<&str, &ProgramDetailsDto> = HashMap::new();
        for detail in &details {
            if let Some(id) = detail.program_id.as_deref() {
                by_id.entry(id).or_insert(detail);
            }
        }
        let with_artwork: Vec<String> = details
            .iter()
            .filter(|detail| detail.has_image_artwork)
            .filter_map(|detail| detail.program_id.clone())
            .collect();
        let images = self.get_image_for_programs(info, &with_artwork).await?;

        // The artwork chosen per program id. Upstream writes it onto the shared
        // details object, so once one airing of a program got it, every later
        // airing of that program carries it too.
        let mut artwork: HashMap<&str, (Option<String>, Option<String>)> = HashMap::new();
        let cache_until = Utc::now() + chrono::Duration::days(MAX_CACHE_DAYS);
        let mut programs = Vec::new();
        for airing in days.iter().flat_map(|day| &day.programs) {
            let Some(program_id) = airing.program_id.as_deref().filter(|id| !id.is_empty()) else {
                continue;
            };
            let detail = *by_id.get(program_id).ok_or_else(|| {
                ServiceError::backend(format!(
                    "Schedules Direct sent no details for program {program_id}"
                ))
            })?;

            // Only images that will be pre-cached, until the token in their
            // URLs can be refreshed (upstream's own note).
            let will_be_cached = airing
                .air_date_time
                .is_some_and(|at| at + chrono::Duration::seconds(airing.duration) < cache_until);
            if will_be_cached
                && let Some(entry) = images.iter().find(|entry| {
                    entry
                        .program_id
                        .as_deref()
                        .is_some_and(|id| program_id.starts_with(id))
                })
            {
                artwork.insert(program_id, chosen_artwork(&entry.data, &token));
            }

            if let Some(program) = program(
                &channel_id,
                airing,
                detail,
                artwork.get(program_id).cloned().unwrap_or_default(),
            ) {
                programs.push(program);
            }
        }
        Ok(Some(programs))
    }

    /// Port of `GetImageForPrograms`: `/metadata/programs/` in batches of
    /// [`IMAGE_BATCH_SIZE`], stopping before the next batch once the image
    /// daily limit is in force.
    ///
    /// An entry carrying an SD code is skipped; a per-entry 5002/5003 inside a
    /// `200` response sets the image limit (jellyfin/jellyfin#17347). A batch
    /// that fails is skipped, as upstream catches it.
    pub(super) async fn get_image_for_programs(
        &self,
        info: &ListingsProviderInfo,
        program_ids: &[String],
    ) -> Result<Vec<super::dto::ShowImagesDto>, ServiceError> {
        if self.is_image_daily_limit_active() {
            return Ok(Vec::new());
        }
        let Some(token) = self.get_token(info).await?.filter(|t| !t.is_empty()) else {
            return Ok(Vec::new());
        };
        if program_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();
        for batch in program_ids.chunks(IMAGE_BATCH_SIZE) {
            // The daily image limit may be surfaced mid-batch.
            if self.is_image_daily_limit_active() {
                break;
            }
            // The trailing slash is SD's documented path (jellyfin/jellyfin#16220).
            let request = SdRequest::new(RequestMethod::Post, "/metadata/programs/", Some(&token))
                .json(to_json(&batch)?);
            match self
                .request::<Vec<super::dto::ShowImagesDto>>(request, true, Some(info))
                .await
            {
                Ok(entries) => {
                    for entry in entries.unwrap_or_default() {
                        let Some(code) = entry.code else {
                            results.push(entry);
                            continue;
                        };
                        tracing::debug!(
                            program = entry.program_id.as_deref().unwrap_or_default(),
                            code,
                            message = entry.message.as_deref().unwrap_or_default(),
                            "Schedules Direct returned an error for a program's images"
                        );
                        if matches!(
                            SdErrorCode::from_code(code),
                            Some(
                                SdErrorCode::MaxImageDownloads
                                    | SdErrorCode::MaxImageDownloadsTrial
                            )
                        ) {
                            tracing::warn!(
                                sd_code = code,
                                "Schedules Direct image download limit: no images until SD resets at 00:00 UTC"
                            );
                            self.set_image_limit_hit();
                        }
                    }
                }
                Err(error) => {
                    // Per station; the airings are still written, without artwork.
                    tracing::debug!(
                        error = %ServiceError::from(error),
                        "could not read program artwork from Schedules Direct; the batch is skipped"
                    );
                }
            }
        }
        Ok(results)
    }
}

/// `GetProgramsAsync`'s input normalisation: the `.json.schedulesdirect.org`
/// suffix dropped (ignoring case), then leading `I`s trimmed, whether or not
/// there was a suffix.
fn normalize_station_id(channel_id: &str) -> String {
    crate::mapping::without_schedules_direct_suffix(channel_id)
        .trim_start_matches('I')
        .to_owned()
}

/// `GetScheduleRequestDates`: every `yyyy-MM-dd` from the earlier of the
/// window start's UTC and server-local wall clock to the later of its end's.
///
/// `.NET` compares the two `DateTime`s by their ticks alone, so the dates are
/// those of the wall clocks, which is what this does with naive times;
/// `local_offset` gives the server zone's offset at an instant.
pub(super) fn schedule_request_dates(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    local_offset: impl Fn(DateTime<Utc>) -> FixedOffset,
) -> Vec<String> {
    let local = |at: DateTime<Utc>| at.with_timezone(&local_offset(at)).naive_local();
    let first = start.naive_utc().min(local(start)).date();
    let last = end.naive_utc().max(local(end)).date();
    let mut dates = Vec::new();
    let mut day: NaiveDate = first;
    while day <= last {
        dates.push(day.format("%Y-%m-%d").to_string());
        let Some(next) = day.succ_opt() else {
            break;
        };
        day = next;
    }
    dates
}

/// The primary and thumb images `GetProgramsAsync` picks from a program's
/// artwork: the best 2:3 image with the title on it (any 2:3 image when none
/// has text), and the best 16:9 one with text, unless it is the primary.
///
/// `BackdropImage` (16:9 without text) is chosen upstream too, but
/// `GetProgram` never copies it into `ProgramInfo`.
fn chosen_artwork(images: &[ImageDataDto], token: &str) -> (Option<String>, Option<String>) {
    let titled: Vec<&ImageDataDto> = images
        .iter()
        .filter(|image| {
            image
                .text
                .as_deref()
                .is_some_and(|text| eq_ordinal_ignore_case(text, "yes"))
        })
        .collect();
    let all: Vec<&ImageDataDto> = images.iter().collect();
    let primary =
        program_image(&titled, 2.0 / 3.0, token).or_else(|| program_image(&all, 2.0 / 3.0, token));
    // Don't supply the same image twice.
    let thumb =
        program_image(&titled, 16.0 / 9.0, token).filter(|thumb| primary.as_ref() != Some(thumb));
    (primary, thumb)
}

/// `GetProgramImage`: the image closest to `aspect`, the tallest of those
/// equally close; its URI when absolute, otherwise an SD image URL carrying the
/// token.
fn program_image(images: &[&ImageDataDto], aspect: f64, token: &str) -> Option<String> {
    let best = images
        .iter()
        .enumerate()
        .min_by(|(ia, a), (ib, b)| {
            let distance = |image: &ImageDataDto| (aspect - aspect_ratio(image)).abs();
            distance(a)
                .total_cmp(&distance(b))
                .then_with(|| size_order(b).cmp(&size_order(a)))
                // `OrderBy` is stable: the first of equals wins.
                .then_with(|| ia.cmp(ib))
        })
        .map(|(_, image)| *image)?;
    let uri = best.uri.as_deref().filter(|uri| !uri.trim().is_empty())?;
    if uri.to_ascii_lowercase().contains("http") {
        return Some(uri.to_owned());
    }
    Some(format!("{API_URL}/image/{uri}?token={token}"))
}

/// `GetSizeOrder`: the height, or 0.
fn size_order(image: &ImageDataDto) -> i32 {
    image
        .height
        .as_deref()
        .and_then(|h| h.parse().ok())
        .unwrap_or(0)
}

/// `GetAspectRatio`: width over height, or 0 when either is missing or zero.
fn aspect_ratio(image: &ImageDataDto) -> f64 {
    let parse = |v: &Option<String>| -> i32 {
        v.as_deref()
            .filter(|s| !s.trim().is_empty())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    };
    let (width, height) = (parse(&image.width), parse(&image.height));
    if width == 0 || height == 0 {
        0.0
    } else {
        f64::from(width) / f64::from(height)
    }
}

/// Whether `list` holds `value`, ignoring case (`IEnumerable.Contains(value,
/// OrdinalIgnoreCase)`: whole elements, not substrings).
fn holds(list: &[String], value: &str) -> bool {
    list.iter().any(|item| eq_ordinal_ignore_case(item, value))
}

/// Port of `GetProgram`. `None` for an airing without an air time.
pub(super) fn program(
    channel_id: &str,
    airing: &ProgramDto,
    details: &ProgramDetailsDto,
    (image_url, thumb_image_url): (Option<String>, Option<String>),
) -> Option<SdProgram> {
    let start = airing.air_date_time?;
    let end = start + chrono::Duration::seconds(airing.duration);
    let program_id = airing.program_id.clone().unwrap_or_default();
    let id = format!(
        "{program_id}T{}C{channel_id}",
        crate::manager::dotnet_ticks(start)
    );

    let audio = program_audio(&airing.audio_properties);

    let is = |value: Option<&str>, wanted: &str| {
        value.is_some_and(|v| eq_ordinal_ignore_case(v, wanted))
    };
    let entity_type = details.entity_type.as_deref();
    let mut is_series = is(entity_type, "episode");
    // It's also a series if it starts with SH.
    if !is_series {
        is_series = starts_with_ignore_case(&program_id, "SH") && program_id.chars().count() >= 14;
    }
    // According to Schedules Direct these are generic, unidentified episodes
    // (`SH005316560000`), so they are keyed on the airing instead.
    let has_unique_show_id =
        !starts_with_ignore_case(&program_id, "SH") || !ends_with_ignore_case(&program_id, "0000");
    let show_id = if has_unique_show_id {
        program_id.clone()
    } else {
        id.clone()
    };

    let official_rating = official_rating(details);
    let overview = overview(details);

    let (mut series_id, mut season_number, mut episode_number) = (None, None, None);
    if is_series {
        series_id = Some(program_id.chars().take(10).collect::<String>());
        if let Some(gracenote) = details.metadata.iter().find_map(|m| m.gracenote.as_ref()) {
            season_number = i32::try_from(gracenote.season).ok();
            if gracenote.episode > 0 {
                episode_number = i32::try_from(gracenote.episode).ok();
            }
        }
    }

    let mut production_year = details.original_air_date.map(|date| date.year());
    if let Some(year) = details
        .movie
        .as_ref()
        .and_then(|movie| movie.year.as_deref())
        .filter(|year| !year.is_empty())
        .and_then(|year| year.parse().ok())
    {
        production_year = Some(year);
    }

    let genres: Vec<String> = details
        .genres
        .iter()
        .filter(|genre| !genre.trim().is_empty())
        .cloned()
        .collect();
    let is_kids = is(details.audience.as_deref(), "children") || holds(&genres, "children");

    Some(SdProgram {
        id,
        channel_id: channel_id.to_owned(),
        start,
        end,
        name: details
            .titles
            .first()
            .and_then(|title| title.title120.clone())
            .unwrap_or_else(|| "Unknown".to_owned()),
        episode_title: details.episode_title150.clone(),
        overview,
        is_news: holds(&details.genres, "news"),
        genres,
        official_rating,
        audio,
        image_url,
        thumb_image_url,
        is_repeat: airing.new.is_none(),
        is_series,
        is_kids,
        is_sports: is(entity_type, "sports"),
        is_movie: is(entity_type, "movie"),
        is_live: is(airing.live_tape_delay.as_deref(), "live"),
        is_premiere: airing.premiere
            || airing
                .is_premiere_or_finale
                .as_deref()
                .is_some_and(|text| text.to_lowercase().contains("premiere")),
        is_hd: holds(&airing.video_properties, "hdtv"),
        is_3d: holds(&airing.video_properties, "3d"),
        etag: airing.md5.clone(),
        show_id,
        series_id,
        season_number,
        episode_number,
        original_air_date: details.original_air_date,
        production_year,
    })
}

/// `GetProgram`'s audio ladder over the airing's `audioProperties`.
fn program_audio(properties: &[String]) -> ProgramAudio {
    if properties.is_empty() {
        ProgramAudio::Stereo
    } else if holds(properties, "atmos") {
        ProgramAudio::Atmos
    } else if holds(properties, "dd 5.1") || holds(properties, "dd") {
        ProgramAudio::DolbyDigital
    } else if holds(properties, "stereo") {
        ProgramAudio::Stereo
    } else {
        ProgramAudio::Mono
    }
}

/// `OfficialRating`: the first content rating's code with `TV` hyphenated
/// (`TVPG` → `TV-PG`), unless it is one of the non-ratings.
fn official_rating(details: &ProgramDetailsDto) -> Option<String> {
    details
        .content_rating
        .first()
        .and_then(|rating| rating.code.as_deref())
        .map(|code| code.replace("TV", "TV-").replace("--", "-"))
        .filter(|rating| {
            !["N/A", "Approved", "Not Rated", "Passed"]
                .iter()
                .any(|invalid| eq_ordinal_ignore_case(invalid, rating))
        })
}

/// `Overview`: the first 1000-character description, else the first
/// 100-character one.
fn overview(details: &ProgramDetailsDto) -> Option<String> {
    details.descriptions.as_ref().and_then(|descriptions| {
        if let Some(long) = descriptions.description1000.first() {
            long.description.clone()
        } else {
            descriptions
                .description100
                .first()
                .and_then(|short| short.description.clone())
        }
    })
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn ends_with_ignore_case(text: &str, suffix: &str) -> bool {
    text.len()
        .checked_sub(suffix.len())
        .and_then(|at| text.get(at..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}
