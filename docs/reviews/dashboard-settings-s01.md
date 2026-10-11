# S01: remote `.strm` shortcut playback

A `.strm` library item stands for a remote stream. Ferrofin treated it as a
local file: its source was the `.strm` path over the file protocol with no
streams, playback info reported nothing playable, and a static stream served
the *text of the `.strm` file* to the player.

Source oracle: Jellyfin `4910aafa1a`: `BaseVideoResolver`/`AudioResolver`
(`IsShortcut`), `ProbeProvider.FetchShortcutInfo` (`ShortcutPath`),
`FFProbeVideoInfo`/`AudioFileProber` (`!IsShortcut || EnableRemoteContentProbe`),
`BaseItem.GetVersionInfo` (remote shortcut arm; local targets refused),
`MediaSourceManager.GetPlaybackMediaSources` (forced remote-probe refresh),
`MediaSourceManager.GetPathProtocol`/`SupportsDirectStream`,
`VideosController` + `FileStreamResponseHelpers.GetStaticRemoteStreamResult`
(static remote relay; other protocols `400`).

## Behavior

- **Scan** (`library_scan.rs`, new `shortcut.rs`): a `.strm` video or audio item
  gets `IsShortcut: true` in its `Data`; when its probe runs the file is read
  and its first line that is not blank or a `#` comment becomes `ShortcutPath`
  — only when it is an `http`, `https`, `rtsp` or `rtp` URL (anything else is
  logged and ignored, as is a file with no usable line; the item keeps the last
  target it had). A scan never probes a shortcut (the ffprobe request is
  dropped), so first scans of `.strm` items no longer run ffprobe on the
  `.strm` file.
- **Remote probe**: `MetadataRefreshOptions.enable_remote_content_probe`
  (`EnableRemoteContentProbe`). A refresh with it probes the target the `.strm`
  names (request path = the URL, protocol from the URL; external sidecars still
  resolve beside the `.strm`) and stores streams, runtime, container and
  chapters like any probe.
- **Media sources** (`media_source_manager.rs`): a video with
  `IsShortcut`+`ShortcutPath` whose target protocol is not `File` is served from
  the target — `Path` = the URL, `Protocol` Http/Rtsp/Rtp, `IsRemote` true, the
  probed container — and `SupportsDirectStream` is re-derived from
  path+protocol for every source (an `.m3u8` target is not direct-streamable).
  A target on the file protocol is never followed. DTO list sources use the same
  builder.
- **Playback** (`GetPlaybackMediaSources`): with a probe allowed, a `.strm`, or a
  video/audio item whose first source lacks a stream of its main type, is
  refreshed `FullRefresh` + remote probe — awaited, through the scanner's
  path-scoped scan, wired in the composition root — and its sources re-read.
- **Delivery** (`streaming.rs`): a static stream of an HTTP(S) source is relayed
  from the target (client `Range` forwarded; upstream status,
  `Accept-Ranges` (`none` when absent), `Content-Range`, `Content-Length` and
  `Content-Type` returned, `application/octet-stream` by default); any other
  non-file protocol is `400 Input protocol … cannot be streamed statically`; an
  unreachable origin is a server error. Transcoding already reads the source's
  path/protocol from the same media source.

## Verification

- Core: scan tests (remote/local/empty/FTP targets: `IsShortcut` always, target
  only when remote, no probe, unchanged files keep it; remote probe reads the
  URL with the Http protocol and not the file, stores a stream and the
  container, picks up a changed target, keeps the last target when the file
  stops naming a remote one, and a non-remote shortcut is never probed even with
  the option); media-source tests (remote target, playlist target, local target
  and stray `ShortcutPath`); playback tests (refresh only with a probe allowed
  and only for a `.strm` or a stream-less video, then sources re-read);
  `shortcut.rs` unit tests (line selection, scheme gate, protocol by prefix).
- API: a loopback origin with `Range` — whole body (`200`, `Accept-Ranges`,
  length), `bytes=2-5` (`206`, `Content-Range: bytes 2-5/10`), RTSP `400`, a
  dead origin `500`.
- Production binary, one `.strm` naming a local origin serving an MP4 and one
  naming `/etc/hostname`, PlaybackInfo then `GET /Videos/{id}/stream?static=true`:

  | | remote `.strm` source | static stream | local-path `.strm` |
  |---|---|---|---|
  | before | local `.strm` path, `File`, no streams, not direct streamable | **the text of the `.strm`** | `.strm` file |
  | after | `http://…/origin.mp4`, `Http`, `IsRemote`, Video+Audio streams, direct stream | **the origin's bytes**, origin hit twice (probe, relay) | unchanged: not followed, no probe |

## Limitations

- Media segments, trickplay and chapter-image extraction already skip
  shortcuts (their own eligibility checks); no change there.
- A forced remote probe re-runs the full metadata refresh of the item (as
  upstream's `FullRefresh` does), so a playback of a stream-less item can reach
  the configured metadata providers.
- Per-source `RequiredHttpHeaders`/User-Agent forwarding for the relay has no
  source of headers for a `.strm`, so none is sent.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on the changed crates passed: core 2,484, api 954, providers
935, extensions 96, livetv 299, wasm 917 (4 skipped), server 237, mediaencoding
205 (real FFmpeg tests enabled; the WASM guest build remains disabled). The first
full run failed the SQL-boundary ratchet because the new media-source tests used
raw SQL; they were rewritten over the shared test helpers, and fmt, Clippy, the
boundary test and core's tests and doctests were rerun clean. Separate line
coverage over each changed crate's own sources: core **95.15%**, api **87.88%**,
providers **91.53%**, all above the 80% gate; traits and the server composition
root (the `RemoteProbe` wiring, exercised by the live run above) are
coverage-exempt. Instrumented builds were cleaned after each crate (`target`
peaked near 30 GiB during the api coverage build and returned to about 25 GiB,
with about 494 GiB free).
