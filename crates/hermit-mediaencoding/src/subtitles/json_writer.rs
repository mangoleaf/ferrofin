//! JSON subtitle writer.
//!
//! Port of `MediaBrowser.MediaEncoding.Subtitles.JsonWriter`. Emits the Jellyfin
//! `{"TrackEvents":[{"Id","Text","StartPositionTicks","EndPositionTicks"}]}`
//! shape the web client consumes, using [`hermit_model::media_info::SubtitleTrackEvent`]
//! as the wire projection of each cue.

use hermit_model::media_info::{SubtitleTrackEvent, SubtitleTrackInfo};

use super::model::Subtitle;

/// Serializes `subtitle` to the Jellyfin JSON subtitle format.
///
/// Port of `JsonWriter.ToText`: projects each [`super::model::Paragraph`] onto a
/// [`SubtitleTrackEvent`] (id from the cue number, tick-based positions) and
/// serializes the `TrackEvents` envelope.
///
/// # Panics
///
/// Panics only if `serde_json` fails to serialize the owned, always-serializable
/// [`SubtitleTrackInfo`] value, which cannot happen for this shape.
#[must_use]
pub fn to_text(subtitle: &Subtitle) -> String {
    let track_events = subtitle
        .paragraphs
        .iter()
        .map(|p| SubtitleTrackEvent {
            id: p.number.to_string(),
            text: p.text.clone(),
            start_position_ticks: p.start_time.ticks(),
            end_position_ticks: p.end_time.ticks(),
        })
        .collect();

    serde_json::to_string(&SubtitleTrackInfo { track_events })
        .expect("SubtitleTrackInfo is always serializable")
}
