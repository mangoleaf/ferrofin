# L26: audio normalization analysis

The scheduled task already consumed the owning library's saved EnableLUFSScan
flag. Album analysis now preserves either existing LUFS or NormalizationGain,
and both album concat input and individual analysis exclude remote protocols.
An ffmpeg start/run failure produces no measurement and lets later tracks run.
The summary parser requires leading whitespace and keeps the first integrated
summary, matching the pinned source's expression.

Source oracle: Jellyfin `4910aafa1a`,
Emby.Server.Implementations/ScheduledTasks/Tasks/AudioNormalizationTask.cs.

Real SQLite/filesystem regressions save the library flag off/on/off/on without
recreating the task, preserve embedded album and track gains, check actual concat
contents, exclude remote tracks, retain previously measured values and continue
after a failed start. The existing positive parser fixture now uses explicit
concatenation so Rust's line continuation cannot strip ffmpeg's indentation.

The native fixture uses two real FLAC tracks, then adds two more. It checks
disabled, enabled, cached repeat, disabled new tracks and enabled new tracks over
HTTP. Album catalog settlement is a separate pass, excluded from scheduled-task
timings: the reference can return stale AlbumNormalizationGain immediately after
analysis. Source pin and native Jellyfin 12.1.0 runtime are distinct. Using
multiple tracks avoids the source's independent single-pending-item batch-save
quirk. These checks establish analysis behavior and returned gains; they make no
claim about every player's use of normalization.

Validation: all five normal and instrumented normalization/parser tests pass,
along with the real lyrics HTTP regression, SQL boundary, formatting, server
build and strict workspace Clippy. Three new test-only lint violations were
corrected before the final passing run. Core independently reaches **85.73%**
line coverage (95,672/111,602), seeding unchanged coverage from L25 and rerunning
all affected tests. LLVM export reports no warnings.

All five after-change native phases complete and match the reference's track
and album gains within 0.00001 after catalog settlement. Before/after task wall
times are **28.32/27.47 ms** disabled, **108.03/132.94 ms** enabled,
**27.90/27.88 ms** cached repeat, **28.22/26.78 ms** disabled with new tracks,
and **84.43/80.94 ms** enabled with new tracks. These single unisolated
observations include 25 ms polling and make no performance improvement claim.
Docker is unavailable. Native ordinary-file checks complement the injected
start-failure/remote-path regressions; they do not reproduce those failures.

Evidence: `/tmp/ferrofin-dashboard-l26-{checks,coverage}.json`,
`/tmp/ferrofin-dashboard-l26-native-{before,after,reference}.json`, and matching
logs/exports. Generated worktree artifacts are **12.39 GiB**; builds remain
serialized under the 30 GiB target cap and 512 GiB host reserve.
