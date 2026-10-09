# T07: rolling HLS segment retention

`EnableSegmentDeletion` and `SegmentKeepSeconds` now control rolling deletion of
HLS segments behind the finalized download position while ffmpeg remains active. This is separate
from the existing end-of-job cleanup. The primary oracle is Jellyfin
`4910aafa1a`, captured in the adjacent `oracle/` manifest and source snapshots.

`TranscodeManager.EnableSegmentCleaning` (lines 570–575) enrolls File or HTTP
inputs, video inputs, HLS output, and known runtime of at least five minutes.
Short video, actual audio inputs, progressive and DASH output, other protocols,
and live sources without a known runtime do not enroll. `IsInputVideo` follows
the input item's media type independently of output selection: audio-only HLS
requested from a qualifying video item can enroll while its ffmpeg stays active.
The separate copy pacing branch still requires a selected video stream. Effective input protocol uses
a nonempty `EncoderPath` together with `EncoderProtocol` when both exist,
otherwise the source's ordinary `Protocol`, as `EncodingHelper.AttachMediaSourceInfo`
does. The cleaner starts only after ffmpeg's first-output wait and only while
the child remains active. It exists even if the enable switch initially is
false, because every callback reads the live encoding settings.

The first and subsequent ticks are 20 seconds apart. Retention is floored at
20 seconds. Position comes from finalized dynamic-HLS response lifetimes, including
aborted responses, not playback-position reports or the furthest produced
segment. `DynamicHlsController` registers `Response.OnCompleted` with the
request's supplied `RuntimeTicks + ActualSegmentLengthTicks`; Kestrel fires that
completion callback when an aborted response lifetime finalizes as well, as
[ASP.NET Core 10 request finalization](https://github.com/dotnet/aspnetcore/blob/v10.0.0/src/Servers/Kestrel/Core/src/Internal/Http/HttpProtocol.cs#L703-L714)
shows. The source
converts ticks to seconds with midpoint ties to even, then computes integer
`(downloadSeconds - max(keepSeconds,20)) / segmentLength`. A positive result
selects inclusive indices zero through that maximum and waits 1.5 seconds
before removing them. A playlist, fMP4 init segment index -1, in-progress
`.tmp` segment and future segment remain. The source selector removes all
case-sensitive occurrences of the playlist stem from each basename and parses
an Int64 index; it does not limit the segment extension. One removal failure
does not skip other selected files. Filesystem/config read failures log at
debug level and do not terminate streaming.

Blocking directory enumeration and removal run in a single blocking batch per
job, so retention does not occupy the HTTP executor or create overlapping
batches when storage is slow. The owned cleaner task ends when it observes ffmpeg's exit. Killing or removing
a job drops and aborts its task, including an in-progress deletion delay. An atomic stop flag also ends a
removal batch between files; one filesystem operation already in progress may
finish, but a removed job schedules no later removals. This explicitly cancelable task
is a Rust lifecycle adaptation; the C# timer disposes future callbacks on stop
but an already dispatched callback may finish its delay. Job teardown keeps its
existing separate `delete_files` behavior.

`EncodingHelper.GetInputModifier` also slows copied video HLS input to
`-readrate 10` when segment deletion is enabled and ffmpeg is at least version
5.0. This command policy is independent of cleaner eligibility. Native-rate
sources instead use `-re`, except RTSP, with precedence over the copy pacing.
At the pinned source's ffmpeg version 8.0 threshold, the corresponding
`-readrate_catchup` is 100 for native rate and 1000 for copy pacing; earlier
versions must not receive this option. Native-rate jobs with deletion disabled
still retain their ordinary `-re` behavior.

Prepared tests cover enrollment boundaries and effective protocol override,
20-second floor, signed/extreme retention and download values, midpoint
rounding and the inclusive deletion boundary, filenames and init/partial files,
continued deletion after a controlled failure, version gates and pacing
precedence, live setting updates, disabled retention, task drop, a canceled pending deletion delay, stopped removal batches and exited
process stop. Tests using a short injected timer exercise the worker lifecycle;
they do not claim to measure the production 20-second interval.

Real verification must run the binary and real ffmpeg with a video at least
five minutes long. Use an inexpensive low-resolution, low-frame-rate clip.
Start a copied video HLS job with deletion enabled to observe `-readrate 10`.
Consume completed segments far enough ahead of a 20-second retained window,
keep the job alive, and record the exact on-disk old/future/init/playlist files
before and after the first production 20-second tick plus 1.5-second delay.
Use a longer clip or native-rate input to keep ffmpeg active throughout. Also
start disabled deletion and short/actual-audio-input/control jobs, change retention and the
enable switch while an eligible job is active, and verify that canceled response finalization advances the supplied dynamic
segment ending position, while canceled progressive reads only count bytes
actually consumed, matching the pinned callback behavior. Stop the actual job and
confirm no further rolling task acts; end-of-job file cleanup is assessed
separately. Native Jellyfin binary evidence is supporting evidence if its
release differs from the primary source pin, and must be labeled accordingly.

The prepared v3 owned HTTP fixture uses a tiny 900-second, 64x64, 1fps clip
and the root's recording encoder without adding pacing. Its separate verifier
checks real copy argv/readrate placement, 13 actual segment responses and
authoritative supplied ending ticks, recorded session pings that keep the 60-second
idle timer from ending the job, live enable and 20/30-second retention
changes across production timer ticks, exact recorded live process argv, and a
post-stop sentinel after 22 seconds. An early elapsed-time guard rejects a
fixture that cannot disable the initial setting before its first tick. The corrected
fixture disables immediately after the first real segment response, before the
remaining twelve reads. VOD playlist delivery alone does not start the reference
encoder; moving the save before that first segment suppressed startup copy pacing
and was independently rejected by the argv assertion. Both failed fixtures are
preserved; the initial draft disabled afterward and its shared-host reference run
took 19.33 seconds, failing the 18-second guard. The exact early save/readback
and timing are now retained. Production timer, deletion and process assertions
remain unchanged; failed evidence is preserved. This is
a prepared verification plan; it has not been executed.

The native draft also assumed a running VOD encoder had written its disk playlist.
FFmpeg writes it at completion; the HTTP playlist and real TS segments are already
available. The fixture now derives the output stem from exact recorded argv and
asserts the expected absent live VOD playlist while retaining all TS, boundary,
process and cleanup proofs. The successful baseline was reverified against this
corrected verifier without rerunning its unchanged observation sequence.

The final sentinel uses numeric basename `0.ts`: the rolling source selector
accepts that index, while ordinary end-of-job cleanup selects paths containing
the job stem. The initial stem-prefixed sentinel was removed by reference final
cleanup, correctly kept separate from rolling deletion. All live retention
phases passed in that preserved attempt; the isolated teardown probe is rerun
with the reference and updated server.

Validation passed:

- fmt: passed (2.18 s, including any compilation).
- http: 1 test run: 1 passed, 0 skipped (25.84 s, including any compilation).
- build: passed (7.82 s, including any compilation).
- native-before: passed (99.54 s, including any compilation).
- native-reference: passed (100.40 s, including any compilation).
- native-after: passed (100.72 s, including any compilation).
- clippy: passed (13.87 s, including any compilation).
- doctests: 3 passed, 0 ignored (11.97 s, including any compilation).
- ferrofin-mediaencoding: 59 tests run: 59 passed, 890 skipped (11.12 s, including any compilation).
- server: 98 tests run: 98 passed, 82 skipped (10.22 s, including any compilation).

Separate changed-crate line coverage:

- ferrofin-mediaencoding: 19,187/21,036 lines (91.210306%), fresh full suite without seed.
  LLVM diagnostics retained: warning: 9 functions have mismatched data.

Actual local native observations (milliseconds):

| Phase / measurement | Before | After | Jellyfin 12.1.0 |
|---|---:|---:|---:|
| startup / startup.startup_ms | 6473.783 | 6488.588 | 5566.607 |
| fixture_media_retention / generation_ms | 67.377 | 55.404 | 53.151 |
| segment_reads / playlist.milliseconds | 6.792 | 3.275 | 64.414 |
| segment_reads / early_disable_save.milliseconds | 9.852 | 7.720 | 14.782 |
| segment_reads / early_disable_encoding.milliseconds | 1.427 | 1.169 | 4.284 |
| disabled_live_tick / save.milliseconds | 7.913 | 9.562 | 4.733 |
| disabled_live_tick / encoding.milliseconds | 2.281 | 3.041 | 4.999 |
| enabled_keep30_live_tick / save.milliseconds | 7.163 | 11.478 | 4.190 |
| enabled_keep30_live_tick / encoding.milliseconds | 2.804 | 2.154 | 7.476 |
| changed_keep20_live_tick / save.milliseconds | 7.976 | 9.756 | 4.784 |
| changed_keep20_live_tick / encoding.milliseconds | 2.408 | 2.024 | 4.649 |
| stopped_timer / stop.milliseconds | 7.198 | 1231.942 | 460.173 |

These are measured request/completion times on a shared host, not publishable benchmark results. The runtime reference is supporting evidence separate from pinned source `4910aafa1a`. No quiet-host latency or GPU certification is claimed.

Actual commands, source hashes, timing and failed attempts: `/tmp/ferrofin-next10-root/evidence/t07/checks.json` and `/tmp/ferrofin-next10-root/evidence/t07/coverage.json`.
All Cargo, native and coverage validation is serialized with one compiler job, disabled incremental/debug data, a 30 GiB target cap and 256 GiB host reserve. Earlier 416 GiB reserve interruptions remain preserved; recalibration is documented against the live node threshold. Source and commits are preserved.
