//! Port of `MediaBrowser.Model.Dlna.ITranscoderSupport`.

/// Capability probe for what a transcoder can produce or extract.
pub trait TranscoderSupport {
    /// Whether the transcoder can encode to the given audio codec.
    fn can_encode_to_audio_codec(&self, codec: &str) -> bool;

    /// Whether the transcoder can encode to the given subtitle codec.
    fn can_encode_to_subtitle_codec(&self, codec: &str) -> bool;

    /// Whether the transcoder can extract the given subtitle codec.
    fn can_extract_subtitles(&self, codec: &str) -> bool;
}
