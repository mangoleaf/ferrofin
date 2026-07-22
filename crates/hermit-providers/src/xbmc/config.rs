//! NFO parser configuration — port of
//! `MediaBrowser.Model.Configuration.XbmcMetadataOptions` (the subset the NFO
//! parsers read) plus the `GetNfoConfiguration` accessor.
//!
//! Upstream this type lives in `MediaBrowser.Model.Configuration` and is fetched
//! via `IConfigurationManager.GetConfiguration<XbmcMetadataOptions>("xbmcmetadata")`.
//! It is server-side configuration plumbing not present in `hermit-model`, so it
//! is re-created here for the parsers.

/// The default release-date format the parsers use for `aired`/`premiered`/
/// `releasedate`/`formed`/`enddate` tags.
///
/// Port of the `ReleaseDateFormat = "yyyy-MM-dd"` set in the C# constructor.
pub const DEFAULT_RELEASE_DATE_FORMAT: &str = "yyyy-MM-dd";

/// Options controlling how NFO metadata is read and written.
///
/// Port of `MediaBrowser.Model.Configuration.XbmcMetadataOptions`, reduced to the
/// fields the parsers actually consult (`UserId`, `ReleaseDateFormat`). The
/// save-time flags (`SaveImagePathsInNfo`, `EnablePathSubstitution`,
/// `EnableExtraThumbsDuplication`) belong to the (deferred) NFO savers and are
/// omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfoConfiguration {
    /// The user whose watched/playcount state `watched`/`playcount`/`lastplayed`
    /// tags apply to (`UserId`). `None`/unparseable disables that import.
    pub user_id: Option<String>,
    /// The `DateTime.TryParseExact` format for release-date tags
    /// (`ReleaseDateFormat`; defaults to `"yyyy-MM-dd"`).
    pub release_date_format: String,
}

impl Default for NfoConfiguration {
    fn default() -> Self {
        Self {
            user_id: None,
            release_date_format: DEFAULT_RELEASE_DATE_FORMAT.to_owned(),
        }
    }
}

impl NfoConfiguration {
    /// Creates the default NFO configuration (matching the C# constructor).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}
