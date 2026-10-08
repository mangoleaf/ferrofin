# L24: lyric provider selection and order

Scheduled lyric downloads now construct automated requests using the live
owning library's DisabledLyricFetchers and LyricFetcherOrder. Disabling ignores
case; configured order matches exact names. Providers first receive stable
intrinsic priorities, with zero as the default, and unranked ties retain that
order. First-success searches run sequentially; all-provider searches run
concurrently and return results in configured provider order.

Manual lyric search deliberately retains the Audio-overload defaults, including
its independent provider selection. Source oracle: Jellyfin `4910aafa1a`,
MediaBrowser.Providers/Lyric/LyricManager.cs constructor and SearchLyricsAsync
overloads, and MediaBrowser.Providers/Lyric/LyricScheduledTask.cs request
construction.

Validation: 28 focused core tests pass, including live nested/adopted ownership,
disabled-name casing, exact order, intrinsic priorities, stable ties and a
barrier that proves concurrent all-provider startup. The task regression checks
that it uses automated search. Real HTTP uses a local LrcLib endpoint: disabled
automatic work makes zero provider requests, manual search still succeeds,
re-enabling downloads actual lyrics, and disabling again stops requests.
The independent metadata HTTP test, complete scan matrix, SQL boundary,
formatting, server build and strict workspace Clippy pass.

Native before/after and Jellyfin 12.1.0 reference checks complete successfully
with three saved disabled-name casings and no lyric files. Before/after task
wall times are 26.93/28.01, 28.72/28.11 and 27.94/28.32 ms. These include
25 ms polling on an unisolated host and make no improvement claim; Docker is
unavailable. External provider access is blocked, so native disabled-task
outcomes alone cannot prove which providers were attempted. The local HTTP
mock supplies that evidence without relying on a public provider. Core coverage
is **94.33%** (104,628/110,918), with all 28 affected tests rerun instrumented
and unchanged coverage seeded from L23. LLVM export emits no warnings.

Evidence: `/tmp/ferrofin-dashboard-l24-{checks,coverage,native}.json`, focused
test logs and native before/after/reference logs. Builds remain serialized with
the 30 GiB target cap and 512 GiB host reserve.
