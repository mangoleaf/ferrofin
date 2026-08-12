//! Port of `Emby.Naming.TV.EpisodeInfo`.

/// Holder object for episode information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeInfo {
    /// The path.
    pub path: String,
    /// The container.
    pub container: Option<String>,
    /// The name of the series.
    pub series_name: Option<String>,
    /// The 3D format.
    pub format_3d: Option<String>,
    /// Whether the file is 3D.
    pub is_3d: bool,
    /// Whether the file is a stub.
    pub is_stub: bool,
    /// The stub type.
    pub stub_type: Option<String>,
    /// Optional season number.
    pub season_number: Option<i32>,
    /// Optional episode number.
    pub episode_number: Option<i32>,
    /// Optional ending episode number (for multi-episode files, e.g. 1-13).
    pub ending_episode_number: Option<i32>,
    /// Optional year of release.
    pub year: Option<i32>,
    /// Optional month of release.
    pub month: Option<i32>,
    /// Optional day of release.
    pub day: Option<i32>,
    /// Whether a by-date expression was used.
    pub is_by_date: bool,
}

impl EpisodeInfo {
    /// Creates a new [`EpisodeInfo`] for the given path.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            container: None,
            series_name: None,
            format_3d: None,
            is_3d: false,
            is_stub: false,
            stub_type: None,
            season_number: None,
            episode_number: None,
            ending_episode_number: None,
            year: None,
            month: None,
            day: None,
            is_by_date: false,
        }
    }
}
