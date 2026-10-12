# Library scan progress verification

Issue [#15](https://github.com/mangoleaf/ferrofin/issues/15). Implemented on
`fix/scan-progress`, starting from `b0e083c6`; reviewed implementation `bd762628`.

## Behavior

Library item progress now comes from a one-second reporter reading completed/total
counters. Start and terminal events bypass that timer. Slow probes keep producing
current progress, including 0%. HTTP virtual-folder responses expose the same
active state and counters. Captured scheduled-task transitions preserve Running/0%
even for scans that finish before the first periodic task update.

The main scheduled-task percentage retains its existing 96% item / 4% closing-pass
mapping. Per-library progress reports the raw completed/total item percentage.
Cancellation and failures clear library activity without reporting successful
completion. Reporter shutdown is explicit after the scan worker stops.

## Independent review

Applied `.claude/skills/review-loop/SKILL.md` after implementation, as requested.
Round 1 identified reporter lifetime, terminal delivery on last-owner drop, and
per-folder snapshot consistency issues. All were corrected and covered by tests.
Round 2 accepted those fixes and identified a test lock-scope lint issue. Round 3
approved the corrected change with no findings. Final verification followed review.

## Automated checks

- `cargo fmt --all --check`: passed.
- `cargo clippy --all-targets --all-features --offline -- -D warnings`: passed.
- `cargo nextest run --workspace --offline`: 7,365 passed; five existing skips.
- `cargo test --workspace --doc --offline`: three passed.
- API line coverage: 86.21% (757 tests passed).
- Core line coverage: 94.08% (1,898 tests passed).

Commands used `RUSTC_WRAPPER=` and the shared `CARGO_TARGET_DIR`. Coverage ran each
crate separately and applied CI's sibling-crate exclusion regex. The initial
unfiltered report included dependencies and cached artifacts from another worktree;
it was not a per-crate measurement. The corrected reports also exclude those other
worktree paths and Rust standard-library paths.

## Live HTTP and WebSocket

`node verify/scan-progress.mjs /home/mango/dev/ferrofin/target/debug/ferrofin-server N`
passed for logging cadences N=0, 1, and 100. Each run created 12 items and held the
first ffprobe process. Start events arrived in 3–27 ms; zero-percent samples followed
at one-second intervals while held. After release, item ratios advanced and terminal
Idle arrived immediately. The run with logging=100 crossed a timer boundary during
finalization and correctly emitted Active/100% followed by Idle/100%.

All three runs also passed live HTTP state, reconnect during the stall, unchanged
sub-second scan start/end, scoped refresh while the global task stayed idle,
cancellation with no late tick, and empty-library start/end assertions. Unchanged
scans took 198–235 ms and emitted both library lifecycle events.

Controlled-clock tests additionally cover exact fractional progress, item 100 having
no special effect, missed ticks, nested and overlapping scans, abort/restart, and
reporter shutdown with a consumer holding a tracker reference.

## Browser

Used the actual local Jellyfin Web 10.11.8 `dist/` with headless Chromium and the
reviewed server, temporary database, and controlled ffprobe. Started the scan by
clicking **Scan All Libraries** in the dashboard. Captured the visible **Scan Media
Library — 0%** indicator, reloaded while the probe was held, and confirmed the same
visible Running/0% state. The library-management page's task bar advanced to 48%
(its whole-task mapping) after release and disappeared on completion. Returning to
the dashboard showed no Running Tasks widget.

This build's React library-management cards do not contain individual refresh
indicators. Source inspection of its legacy/other card handlers confirms that they
hide incoming values of exactly zero. No claim is made that those cards now remain
visible at zero, or that the issue reporter's Web 12.1 build was browser-tested.

Screenshots and DOM captures are under `/tmp/ferrofin-scan-progress/`, including
`browser-zero.png`, `browser-reload.json`, and `browser-library-samples.json`. The
temporary server and browser were stopped after the checks.

## Before/after overhead

Three alternating baseline/fixed runs on generated 200-item libraries, logging=100,
probe concurrency=1, zero-delay ffprobe stub, fresh database per run. Compared the
saved `b0e083c6` debug binary with the reviewed `bd762628` debug binary, after builds
and tests ended. Wall time spans task submission through observed completion.

| Scan | Baseline median (range), ms | Fixed median (range), ms | Median change |
| --- | --- | --- | --- |
| Initial, 200 new items | 552 (541–561) | 562 (545–608) | +1.7% |
| Unchanged, 200 skipped items | 268 (260–273) | 256 (254–294) | −4.5% |

Ranges overlap; these small local debug-build measurements do not establish a
production throughput change. Each sub-second run emitted two library events in
both builds. Baseline emitted at item 100 and completion; fixed emitted start and
completion. The held 12-item fixture emitted five or six messages over about four
seconds, demonstrating that event volume now follows elapsed time rather than item
count. Raw runs are `/tmp/ferrofin-scan-progress/measure-{before,after}-{1,2,3}.json`.

Reproduce with `node verify/scan-progress.mjs BINARY 100 --measure`.

## Scope limits

The inspected stock Jellyfin Web handlers hide incoming library-card progress at
exactly 0% and 100%, independently of RefreshStatus. This server fix sends accurate
values and lifecycle state. Full card visibility at zero or during finalization
requires a client change; no fabricated positive percentage is sent. The open
client decision remains in `brain/plans/PLAN_SCAN_PROGRESS.md`.

## Web 12.0 follow-up (#15)

The deployed Docker image bundles Web 12.0; the original browser verification used
10.11.8. Built the exact `jellyfin-web` v12.0 source in `/tmp` with its lockfile
(SDK `0.0.0-unstable.202607220710`) and reproduced the missing live updates.

### Root cause

Web 12.0 subscribes to `ScheduledTasksInfo` through the SDK, opening
`/socket?ApiKey=...` and sending
`{"MessageType":"ScheduledTasksInfoStart","Data":"0,1000"}`. The server's
`canonicalize_path_case` middleware changes `ApiKey` to `apiKey`. The socket
handler matched only `ApiKey` or legacy `api_key`, so the connection upgraded
anonymously (`authenticated=false`, `reason=no-token`). It silently ignored the
subscription. HTTP task reads could briefly show a bar or restore one on reload,
but no live task updates arrived: the browser stayed at 0% after the scan finished.

The fix matches token query keys without regard to ASCII case, preserving the
case-sensitive token value and existing legacy/header precedence. No client
change or progress-value workaround is needed. The unit regression covers the
SDK, normalized, lowercase, uppercase, legacy and empty query forms. The live
fixture now uses the SDK's `ApiKey`/`0,1000` protocol through the complete server,
then reconnects with the legacy protocol. Before the fix it timed out waiting for
its first task snapshot, with an anonymous socket in the server log.

### Temporary diagnostics

At INFO, `publishing library scan progress` records every timer sample and
lifecycle update: library, scan generation, completed, total, phase, percentage
and refresh status. Existing item-count logs remain. Publication errors now warn.
The reporter has its own root span.

`dashboard task subscription requested` records whether a task subscription was
authenticated and whether it starts/stops streaming. `dashboard scan task snapshot`
records the running/cancelling RefreshLibrary task state and percentage inside the
socket's session span. This means a snapshot was prepared, not that the browser
received it. These diagnostics are marked temporary for #15; remove or lower them
after homelab confirmation. Tokens and raw socket URLs are never logged.

Web 12.0's main dashboard and Libraries page render the `RefreshLibrary` task when
`State` is `Running`; the individual library cards do not render `RefreshProgress`.
A single-library item refresh takes a separate path in both servers and does not
start the global scheduled task. This is distinct from the authentication bug.

### Follow-up review and verification

Independent review found one issue in the verification script: measurement mode
also switched to SDK authentication, which the old baseline cannot accept. Kept
legacy authentication for `--measure` only. Round 2 approved. Final Clippy caught
the socket handler's length after adding diagnostics; extracted the subscription
log into a helper. Round 3 approved with no findings.

After final review:

- Formatting and API/core all-target, all-feature Clippy passed.
- Rebuilt binary passed the full SDK/legacy HTTP/WebSocket fixture. Start at 0%,
  one-second stalled ticks, completed/total ratios, terminal state, reconnect,
  cancellation, scoped refresh, unchanged and empty scans passed.
- Actual Web 12.0 in Chromium: main dashboard showed the scan at 0%, including
  reload while the first probe was held. Libraries page advanced through
  32%, 48%, 64%, and 80%, then removed the bar at completion. Browser verification
  used the same authentication fix before the final logging-helper extraction.
- Actual library menu -> Scan library -> default scan mode: server reported
  Active/0%, then Active/33.33%, while RefreshLibrary remained Idle and no bar
  appeared. Confirmed this matches the upstream provider-queue path and Web 12.0
  LibraryCard source. No client files changed.
- Disposable browser and fixture stopped after verification.

- API: 757 tests passed, 86.17% line coverage.
- Core: 1,898 tests passed, 94.08% line coverage.
- Both passed CI's 80% per-crate line gate with sibling-crate/source exclusions.

Three alternating debug-build measurements on the same disposable 200-item fixture,
using legacy socket authentication for both binaries so the old bug does not prevent
measurement: initial-scan median 560 -> 555 ms (before range 560–580; after 541–564),
unchanged-scan median 254 -> 268 ms (before 238–288; after 254–268). Ranges overlap;
this small local sample does not establish a timing regression. These sub-second
scans measure lifecycle logging, not sustained tick-log overhead. The held 12-item
live fixture separately verified every one-second sample and its diagnostic line.
Raw results are `/tmp/ferrofin-scan-progress/web12-measure-{before,after}-{1,2,3}.json`.

All follow-up changes remain on `fix/scan-progress`; homelab deployment confirmation
is still needed. The logging is intentionally temporary.


## Diagnostic logging cleanup

After homelab confirmation of full scans on the dashboard and individual scans on
Home -> My Media, retained the three detailed progress/subscription diagnostics at
DEBUG instead of INFO. Existing scan lifecycle and every-100-item INFO messages,
WebSocket connection logs, and progress-publication warnings keep their levels.
