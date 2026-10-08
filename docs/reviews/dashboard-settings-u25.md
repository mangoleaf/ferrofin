# U25: access schedules

Current Jellyfin Web rebuilds schedule rows from three HTML attributes:
`DayOfWeek`, `StartHour` and `EndHour`. Both hours are strings; neither database
`Id` nor `UserId` is sent. Ferrofin required those IDs and returned HTTP 400 for
any nonempty schedule list, despite enforcing schedules sent by older/full DTO
clients. `AccessSchedule` now accepts upstream constructor defaults. Persistence
continues assigning the row identity and target user inside the policy transaction.
The existing numeric-body binder handles Web's quoted hours.

Login and ordinary authorization also stopped at whole-second precision. They
now include fractional seconds, matching `TimeOfDay.TotalHours`: the exact end
is allowed, but 100 ns after it is outside the window.

Source: Jellyfin `4910aafa1a`, `AccessSchedule`,
`UserEntityExtensions.IsParentalScheduleAllowed`, `UserManager.AuthenticateUser`,
`DefaultAuthorizationHandler`; Web `1e507c588f`,
`apps/dashboard/features/users/components/ParentalControl.tsx:getSchedulesFromPage`.
Native Jellyfin 12.1.0 confirms all 16 focused save/enforcement observations.

Existing U07 semantics remain: server-local time, no schedules means unrestricted,
any matching window permits access, day groups are honored, and overnight windows
are not implicitly split. Existing administrator tokens bypass the default-route
schedule check; password login still checks their schedule. User-detail and
system-info routes keep upstream's schedule exceptions. Policy saves take effect
on the next request without replacing the token.

Validation passes: 55 focused core tests, the model payload regression, all 892
API tests, ten real-manager HTTP tests, SQL boundary, formatting, strict workspace
Clippy and server build. Boundary regressions cover the exact start/end and the
first 100 ns past the end; HTTP verifies that saved rows receive real IDs and
that changing a schedule affects an already issued token immediately.

All **49 native observations pass** after the fix. Before it, all six nonempty
Web payloads returned 400; the baseline then supplied full legacy rows to check
authorization independently. After it, all eight saves (including clears)
return 204 without that fallback. Saved day groups, multiple windows, resets,
login, existing tokens and administrator exceptions match the pinned rules.

Fifty warm scheduled `GET /Users/Me` requests after ten warmups measured median
**0.460 → 0.360 ms**, p95 **0.625 → 0.501 ms**. These debug builds ran on a shared
host while other checks compiled; this is path verification, not a production
performance claim. Native fixtures use `TZ=America/Denver` for both client and
server. Evidence: `/tmp/ferrofin-dashboard-u25-{before,after}-results.json`,
`/tmp/ferrofin-dashboard-u25-checks.json`, and the reference fixture
`/tmp/ferrofin-dashboard-u25-reference-jellyfin-2rb2_mkh/results.json`.

Separate coverage gates pass: **core 94.61%** (2,149 tests), **model 88.58%**
(959 tests), **API 87.52%** (892 tests). All coverage test runs pass. LLVM reports
18, 39 and 81 mismatched-function warnings respectively; the threshold results
retain that measurement limitation. The private workspace coverage cache was
cleaned before this sequence. Evidence:
`/tmp/ferrofin-dashboard-u25-coverage.json` and its referenced logs.
