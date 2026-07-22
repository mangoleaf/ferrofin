//! Port of `Emby.Naming.TV.EpisodePathParserResult`.

/// Holder object for an [`crate::tv::EpisodePathParser`] result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodePathParserResult {
    /// Optional season number.
    pub season_number: Option<i32>,
    /// Optional episode number.
    pub episode_number: Option<i32>,
    /// Optional ending episode number (for multi-episode files, e.g. 1-13).
    pub ending_episode_number: Option<i32>,
    /// The name of the series.
    pub series_name: Option<String>,
    /// Whether parsing was successful.
    pub success: bool,
    /// Whether a by-date expression was used.
    pub is_by_date: bool,
    /// Optional year of release.
    pub year: Option<i32>,
    /// Optional month of release.
    pub month: Option<i32>,
    /// Optional day of release.
    pub day: Option<i32>,
}
