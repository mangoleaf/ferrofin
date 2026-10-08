# U19: library selection and direct item access

Integrated main's visibility merge `f366059b` (PR #43), preserving the first
30 settings fixes. User-scoped item actions now evaluate the item, its parents
and its library membership before reading or mutating data. The evaluator
handles adopted physical folders, shared locations, owner extras, cached images,
private playlists and batched SyncPlay queues. Administrators remain subject to
their own item visibility; explicit target-user requests use that user's rules.

The integration retains U12's item download eligibility and requested-version
file selection, U15's subtitle permission, and the earlier combined default and
administrator policies. It also corrects three interactions:

- Public image/attachment routes resolve authenticated identity without applying
  the default route policy. A denied access schedule must not erase identity and
  turn a restricted user's request into unrestricted anonymous image access.
- Userless PlaybackInfo can return source information. Negotiation with a device
  profile still requires a user, matching upstream's empty-ID 400 and missing
  user 404. Explicit API-key target users retain their playback policy.

- Posted PlaybackInfo `UserId` now selects the effective user; the query parameter
  takes precedence, and ordinary callers cannot select another account.

Compared with Jellyfin `v12.0-rc7` (`4910aafa1a`), `LibraryManager`, item-kind
visibility overrides, `ImageController`, `MediaInfoController` and
`MediaInfoHelper`. The existing [visibility investigation](../investigations/per-user-item-visibility.md)
records the route-by-route oracle and its separate live Jellyfin 12.1 evidence.

Preserved upstream exceptions include public userless images, raw streaming
lookups, explicit-ID browse queries and the 401 result for a hidden browse
parent. This is not a claim that every media URL returns 404 when hidden.
`BlockedMediaFolders` is a legacy stored preference that takes precedence over
the allow-list; upstream's current policy update does not write that obsolete
field, and the Web editor sends null. New selections use `EnableAllFolders` and
`EnabledFolders`. The independent unrated/tag browse and rating-dataset findings
remain U22–U24; library enable behavior is L03.

Validation: all 21 core visibility tests, four real-manager HTTP tests, 891 API
tests, SQL boundary checks, formatting, strict workspace Clippy and server build
pass. The public image/schedule and posted target-user regressions pass. The
native server matches all 33 observations in the recorded Jellyfin 12.1 fixture.
Hidden item reads/mutations return 404, cached images remain hidden, API-key
explicit users are checked, and a denied delete preserves the item.

Measured native debug-build median milliseconds (50 requests per route, identical
synthetic libraries) before → after: detail 3.281 → 3.571; PlaybackInfo
1.136 → 2.752; cached image 0.662 → 1.874; SyncPlay list 0.963 → 1.893. These
added visibility reads have a measurable cost. The shared host and debug builds
are unsuitable for a production performance claim; Docker benchmarking is
unavailable. Raw fixtures: `/tmp/ferrofin-visibility-ferrofin-ba3fp826` and
`/tmp/ferrofin-visibility-ferrofin-d8kmrtfs`; checks:
`/tmp/ferrofin-dashboard-u19-checks.json`. Broader coverage/workspace checks will
run at the batch checkpoint; the preceding checkpoint's host inotify exhaustion
remains recorded separately.
