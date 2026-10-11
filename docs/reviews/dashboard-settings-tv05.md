# TV05: tuner stream settings reach the HLS transcode

A tuner's **Enable stream looping**, **Ignore DTS** and **Read input at native
frame rate** settings reached the media source (and the DVR recorder), but the
general HLS transcode planner consumed only the native-rate flag (as `-re`).
Playing a live channel through HLS therefore ignored the other two, and the
input timestamp sync a native-rate source needs.

Source oracle: Jellyfin `4910aafa1a`, `EncodingHelper.GetInputModifier` and
`AttachMediaSourceInfo` (`ReadInputAtNativeFramerate`, `IgnoreInputDts` →
`+igndts`, `RequiresLooping` → `-stream_loop -1 -reconnect_at_eof 1
-reconnect_streamed 1 -reconnect_delay_max 2`, `InputAudioSync = "1"` and
`InputVideoSync = "-1"` for native-rate sources that are not segmented live
streams or for a local `wtv` file).

## Behavior

The input options before `-i` now follow `GetInputModifier`'s order:

1. **Timestamp sync** — `-async 1`, and `-vsync -1` below ffmpeg 5.1 (where
   `-fps_mode` cannot be an input option), for a native-rate source that is not
   a segmented live stream, and for a local `wtv` file.
2. **Pacing** — unchanged (`-re`, or the copy `-readrate` pacing, with
   `-readrate_catchup` on ffmpeg 8).
3. **`-fflags +igndts`** when the source ignores input DTS.
4. **Looping** — `-stream_loop -1 -reconnect_at_eof 1 -reconnect_streamed 1
   -reconnect_delay_max 2` when the source requires looping.

## Verification

- Unit (mediaencoding): the sync options for native/non-native, segmented live,
  `wtv` and the ffmpeg 5.1 boundary; ignore-DTS and looping alone and together.
- Planner (server): a source with all three flags on ffmpeg 5.0 yields
  `-async 1 -vsync -1 -re -fflags +igndts -stream_loop -1 …` before `-i` and
  none of it after.
- Production binary, previous baseline vs updated, an M3U tuner with stream
  looping, ignore DTS and native rate over a local MPEG-TS stream, opened
  through `PlaybackInfo` and `live.m3u8` with ffmpeg's argv recorded:

  | | input options before `-i` |
  |---|---|
  | before | `-analyzeduration 200M -probesize 1G -re -readrate_catchup 100` |
  | after | same, then **`-fflags +igndts -stream_loop -1 -reconnect_at_eof 1 -reconnect_streamed 1 -reconnect_delay_max 2`** |

  A segmented live stream takes no `-async`, as upstream. The playlist request
  itself answered 500 on both builds in this harness; its cause was not
  investigated, and it is identical before and after, so the recorded argv is the
  evidence.

## Limitations

- The other `-fflags` members upstream's modifier can add (`+ignidx`,
  `+genpts` for copy, `+discardcorrupt`, `+fastseek`) and the `-f` input
  container option are not driven by these settings and are unchanged here.
- Direct play of a looping tuner is already refused by the media source
  (`SupportsDirectPlay` is false), so looping sources always reach this path.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on mediaencoding passed: mediaencoding 954, hls 117, core
2,447, extensions 96, livetv 299, wasm 62, server 231. Real FFmpeg tests
enabled; the WASM guest build remains disabled. Line coverage over
mediaencoding's own sources: **94.22%**, above the 80% gate; traits and server
composition are coverage-exempt. The instrumented build was cleaned right after
the numbers were taken, leaving `target` at about 24 GiB with about 494 GiB
free.
