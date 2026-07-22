//! Port of `Emby.Naming.TV.SeriesPathParserResult`.

/// Holder object for an [`crate::tv::series_path_parser::parse`] result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeriesPathParserResult {
    /// The name of the series.
    pub series_name: Option<String>,
    /// Whether parsing was successful.
    pub success: bool,
}
