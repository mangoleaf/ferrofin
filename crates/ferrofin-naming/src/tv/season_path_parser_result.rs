//! Port of `Emby.Naming.TV.SeasonPathParserResult`.

/// Data object carrying the result of [`crate::tv::season_path_parser::parse`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeasonPathParserResult {
    /// The season number.
    pub season_number: Option<i32>,
    /// Whether parsing was successful.
    pub success: bool,
    /// Whether the path is a season folder.
    pub is_season_folder: bool,
}
