# Dashboard settings: second implementation batch

This batch completes **U08–U12 and U14–U18**, ten more findings after the first
20, on `fix/dashboard-settings`. Each has one signed-off commit without a
co-author trailer. U13 has a separate dependency review and remains open. The
canonical checklist is `brain/knowledge/JELLYFIN_WEB_DASHBOARD_SETTINGS_REVIEW.md`.

| Findings | Result |
|---|---|
| [U08](dashboard-settings-u08.md) | Saved playback permission reaches item PlayAccess; session Play enforcement verified. |
| [U09](dashboard-settings-u09.md) | Trusted transcode permissions reach stream planning and separate cached jobs. |
| [U10](dashboard-settings-u10.md) | Remote-source transcoding policy reaches negotiation and stream-copy decisions. |
| [U11](dashboard-settings-u11.md) | User/server bitrate caps use trusted network classification and affect actual FFmpeg output. |
| [U12](dashboard-settings-u12.md) | Download eligibility agrees with DTOs, and alternate versions download their own bytes. |
| [U14](dashboard-settings-u14.md) | Existing collection-management policy verified over real HTTP. |
| [U15](dashboard-settings-u15.md) | Subtitle management enforces the saved permission; deletion retains upstream administrator restriction. |
| [U16](dashboard-settings-u16.md) | Existing independent Live TV access/management policies verified. |
| [U17](dashboard-settings-u17.md) | Command delivery and session mutations enforce control rights; cast lists honor shared-device and device-access rules. |
| [U18](dashboard-settings-u18.md) | SyncPlay preserves inherited administrator access and matches empty-user-ID errors. |

The behavior oracle is Jellyfin `v12.0-rc7`, `4910aafa1a`; the Web reference
remains `1e507c588f353482a00333f84e36ddb7c8fc8221`. Several original audit claims
were corrected after tracing upstream authorization and playback code: these
settings do not all impose a universal prohibition on direct stream URLs.

## Validation

Formatting, strict workspace Clippy with all targets/features, the SQL boundary,
the server build and all **three doctests** pass. Focused tests and native HTTP,
WebSocket and FFmpeg checks are recorded per finding. Final API validation is
**889 tests passed**, with **87.65% line coverage**. Core line coverage is
**90.69%**. Each report excludes other workspace crates and exceeds the 80%
threshold. These are the two non-exempt crates changed in this batch; traits and
the server composition root have the repository's coverage exemption.

**The workspace gate is not green.** Its latest run executed 7,786 tests:
7,781 passed, five failed, and five additional tests were skipped. One failure
was an older malformed-subtitle fixture lacking U15's newly required permission.
That fixture is corrected in U15's commit; all 14 video tests and the entire
889-test API suite subsequently pass. No production code changed after that
workspace run. The remaining four failures are:

- Core `notify_watcher::tests::a_metadata_only_change_of_a_root_is_not_reported`.
- Mediaencoding `transcoding::fs_wait::tests::wakes_on_file_creation_well_before_the_fallback_tick`.
- Mediaencoding `transcoding::fs_wait::tests::change_before_wait_is_not_lost`.
- Server `scan_change_detection::a_scan_reprocesses_only_what_changed`.

The host has exhausted its inotify watch allowance. Direct `inotify_add_watch`
calls on `/tmp` and a fresh temporary directory return `ENOSPC`, including
outside the sandbox. The core and server logs explicitly report the watch
limit; the mediaencoding tests reach their fallback timer instead of receiving
watch events. The separate core coverage run passed 2,072 tests and failed four
watcher tests. Its percentage threshold passes, but its combined test/coverage
command does not. No limits were changed, tests weakened or unrelated processes
stopped. Rerun the workspace and core coverage gates when watch capacity is
available. The earlier U08–U11 checkpoint had seven failures from the same
watcher dependency; variation reflects shared-host availability.

All live checks used disposable accounts, databases and media. Per-finding
before/after timings are recorded as shared-host observations, with no speedup
claim. Docker access remains unavailable and the host cannot supply the quiet
benchmark conditions required for a combined performance result. The previous
batch's functional browse comparison does not measure this batch's latency.

## Open work

[U13](dashboard-settings-u13.md) depends on the existing physical-deletion
workstream and its scanner prerequisites; permission checks alone do not close
it. U19 is next: integrate main's visibility merge `f366059b` (PR #43), preserving
the settings fixes and validating their interactions. Its parent `ba619e65` is
itself a JSON-binding merge, not the visibility implementation to cherry-pick.

New findings S01 (STRM source resolution/probing) and S02 (audio-specific device
profile planning) remain explicit work items in the living document. P01's
server bitrate consumer is fixed by U11, but its separate dashboard-section
review is still pending. No other setting is marked complete by implication.

The [validation summary](dashboard-settings-batch-02-validation.json) preserves
test totals, coverage outcomes, native probe counts and timings. Raw local logs
use `/tmp/ferrofin-dashboard-batch02-*`; per-finding live results use
`/tmp/ferrofin-dashboard-u*-results.json`.
