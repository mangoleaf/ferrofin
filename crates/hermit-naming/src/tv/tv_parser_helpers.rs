//! Port of `Emby.Naming.TV.TvParserHelpers`.

use hermit_model::entities::SeriesStatus;

const CONTINUING_STATE: [&str; 3] = ["Pilot", "Returning Series", "Returning"];
const ENDED_STATE: [&str; 2] = ["Cancelled", "Canceled"];

/// Tries to parse a string into a [`SeriesStatus`].
///
/// Returns `Some(status)` on success, mirroring the C# `TryParseSeriesStatus`
/// bool + out-param.
#[must_use]
pub fn try_parse_series_status(status: Option<&str>) -> Option<SeriesStatus> {
    let status = status?;

    if let Some(parsed) = parse_enum(status) {
        return Some(parsed);
    }

    if CONTINUING_STATE
        .iter()
        .any(|s| s.eq_ignore_ascii_case(status))
    {
        return Some(SeriesStatus::Continuing);
    }

    if ENDED_STATE.iter().any(|s| s.eq_ignore_ascii_case(status)) {
        return Some(SeriesStatus::Ended);
    }

    None
}

/// Case-insensitive parse of the canonical [`SeriesStatus`] variant names,
/// mirroring `Enum.TryParse(status, true, out _)`.
fn parse_enum(status: &str) -> Option<SeriesStatus> {
    if status.eq_ignore_ascii_case("Continuing") {
        Some(SeriesStatus::Continuing)
    } else if status.eq_ignore_ascii_case("Ended") {
        Some(SeriesStatus::Ended)
    } else if status.eq_ignore_ascii_case("Unreleased") {
        Some(SeriesStatus::Unreleased)
    } else {
        None
    }
}
