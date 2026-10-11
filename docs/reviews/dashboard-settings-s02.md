# S02: audio-specific device-profile negotiation

Playback negotiation always ran the *video* stream builder. A music track
therefore went through the video decision (HLS pin, `/videos/{id}/master.m3u8`
URL, video-container rules) even when the device directly played its format,
and a cover-art "video" stream of an audio file did not help.

Source oracle: Jellyfin `4910aafa1a`: `MediaInfoHelper.SetDeviceSpecificData`
(`item.MediaType == Audio ? GetOptimalAudioStream : GetOptimalVideoStream`, and
the shared capability/URL block), `StreamBuilder.GetOptimalAudioStream`,
`GetAudioDirectPlayProfile`, `IsAudioContainerSupported`,
`IsAudioDirectPlaySupported`/`IsAudioDirectStreamSupported`,
`GetTranscodeReasonsFromDirectPlayProfile`.

## Behavior

- `ferrofin-model` `StreamBuilder::get_optimal_audio_stream` (+ the audio
  direct-play helpers), a faithful port:
  - forced direct play / direct stream short-circuit;
  - the default audio stream (an item with none offers nothing — upstream
    throws there);
  - a direct-play profile that names the container *and* the audio codec, with
    the Matroska/WebM rule (`mkv,webm` is not played by a `webm`-only device);
  - failing that, a profile whose container is not the item's but whose audio
    codec (or bare container name) is — a **direct stream**, reason
    `ContainerNotSupported`; a bitrate over the limit adds
    `ContainerBitrateExceedsLimit`; with no profile, reasons come from the
    profile list;
  - direct play also needs the codec conditions to pass
    (`GetCompatibilityAudioCodec`: channels, bitrate, sample rate, bit depth);
  - a direct stream remuxes into `ts` (or the profile's `ts`/`mp4`), refused
    over HLS when the profile's codec is not one HLS carries
    (`ts`: aac/ac3/eac3/mp3; `mp4` adds alac/flac/opus/dts/truehd);
  - otherwise the first audio transcoding profile the transcoder can encode to:
    its container/protocol, the codec conditions, `GlobalMaxAudioChannels`, and
    the bitrate `min(configured, requested ?? music-streaming bitrate ?? 128 k)`
    capped by what the source condition left.
- The playback-info handler (`media_info.rs`) uses it for items whose media type
  is `Audio`: direct play stays direct play; anything else becomes an audio
  transcode at `/audio/{id}/stream.{container}` (or `master.m3u8` when the
  profile says HLS) with the profile's sub-protocol and container, `Allow*Copy`
  vetoes appended, `SupportsDirectPlay/DirectStream` cleared. There is no HLS
  pin for audio. A source that cannot transcode (permission or client veto)
  yields no decision, exactly as upstream's `null` leaves the static source.

## Verification

- Model: eleven cases over music-client profiles (direct play, unsupported
  format → mp3 transcode with the music bitrate, bitrate over limit, requested
  bitrate capped by the limit, codec-condition failure, container remux, HLS
  codec restriction in `ts` vs `mp4`, Matroska/WebM, forced methods, no audio
  stream / no permitted transcode, profile the transcoder cannot encode).
- API handler test: direct play with cover art stays direct; FLAC transcodes at
  `/audio/…/stream.mp3` with the codec, container and sub-protocol; copy vetoes;
  permission and client vetoes produce no URL.
- Production binary, a FLAC (with embedded cover art) and an MP3 under a profile
  that direct plays MP3 and transcodes to MP3, PlaybackInfo then a GET of the
  returned URL:

  | | FLAC track | MP3 track |
  |---|---|---|
  | before | `/videos/…/master.m3u8` (HLS video master), `ts` | **same HLS URL, direct play denied** |
  | after | **`/audio/…/stream.mp3`, http**; the URL serves 49 KB of MP3 (ffprobe `mp3`) | **direct play, no transcode** |

## Limitations

- An audio item with no probed audio stream gets no negotiation (upstream
  throws); it keeps its static source.
- `DirectStream` for audio is reported as a transcode URL because the server
  keeps `EnableDirectStream` off (upstream's "direct-stream http streaming is
  currently broken" rule); the `/audio` route's remux itself is unchanged.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on the model crate passed: model 992, api 936, core 2,484,
providers 917 (4 skipped), mediaencoding 954, livetv 299, server 237, hls 117,
extensions 96, wasm 62; real FFmpeg tests enabled, the WASM guest
build disabled. Two lints and an old assertion changed along the way: the new
`clone_into` and an over-long helper (split into `optimal_stream`), and
`negotiated_capabilities_respect_every_playback_permission_combination`, which
had pinned the video builder's behavior for audio (an audio source that cannot
transcode now gets no decision and keeps the static source's flags, as
upstream's `null`); after those, fmt, Clippy, the boundary test and the model and
api tests and doctests were rerun clean. Separate line coverage over each changed
crate's own sources: model **89.76%**, api **87.95%**, both above the 80% gate.
Instrumented builds were cleaned after each crate (`target` about 25 GiB).
