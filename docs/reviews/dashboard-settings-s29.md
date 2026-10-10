# S29: the metadata refresh queue

`ProviderManager.QueueRefresh` is a serial, priority-ordered queue.
Ferrofin's `queue_refresh` ignored the priority and spawned a task per
request, so a burst of refreshes ran concurrently in no defined order, each
under whatever culture its own requester carried.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/Manager/ProviderManager.cs` — `QueueRefresh`,
`StartProcessingRefreshQueue`, `GetRefreshQueue`; `RefreshPriority`
(`High = 0`, `Normal = 1`, `Low = 2`).

## Behavior

- **One worker, priority order.** Requests join a queue ordered by priority
  (`High` first) and drained by one worker, one refresh at a time. Equal
  priorities run in arrival order; .NET's `PriorityQueue` leaves that order
  unspecified, so arrival order is one valid choice.
- **First-starter context.** The worker is started by the first request that
  finds none running, so the whole drain, including refreshes queued later by
  other callers, runs under the first requester's culture, as upstream's
  `Task.Run` captures it.
- **Start/stand-down under one lock.** The worker takes the next item and
  stands down under the same lock that requests use to join, so a request
  queued as the queue runs dry starts a new worker rather than waiting on one
  that has stopped. A panicking refresh stands the worker down too (a drop
  guard), so the queue cannot be wedged; items still waiting start with the
  next request.
- **Failures keep draining.** A failed refresh is logged and the next one runs.
- An empty item id is rejected (`ArgumentException`), and `get_refresh_queue`
  returns the ids waiting (previously always empty), without the one being
  refreshed, as upstream's `GetRefreshQueue`.

## Verification

- Four requests (Low, Low, High, Normal) enqueued before the worker's first
  turn run as High, Normal, Low, Low in arrival order, while `get_refresh_queue`
  lists all four waiting and is empty after the drain.
- An empty id is `InvalidInput`; after the queue drains and the worker stands
  down, the next request starts a new worker.
- Three requests under cultures `sv`, `de`, `fr` (the first refresh fails)
  all run under `sv`, and the failure does not stop the others.
- Production binary, previous baseline vs updated, 300 movies, 300 full
  refresh requests in a row: all `204`, accepted in 0.61 s before and 0.65 s
  after, and the queue drained in about the same time (62.3 s before, 61.1 s
  after — dominated by the polling used to detect completion, so a sanity
  check, not a benchmark). Ordering and context are not observable over HTTP
  because every route queues at `High`; they are covered by the tests above.

## Limitations

- No `Dispose` seam exists on the provider manager, so upstream's
  cancel-on-dispose path has nothing to attach to; shutdown ends the process.
- `MusicArtist` items use `RefreshArtist` upstream; Ferrofin's refresh already
  routes every item through `refresh_full_item`, unchanged here.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on providers passed: providers 899 (4 existing skips),
core 2,432, livetv 296, extensions 96, wasm 62, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first run exposed an
older test that queued a nil id and expected success (upstream refuses it; the
test now queues a real one) and a Clippy `missing_fields_in_debug` on the new
field; the providers suite, Clippy and coverage were rerun on the fixed tree,
while the downstream package results were taken before those test-only and
`Debug` edits. Line coverage over providers' own sources: **91.40%**, above the
80% gate; traits and server composition are coverage-exempt. The instrumented
build took `target` to 28.3 GiB; its cache was removed after the numbers,
leaving 24.0 GiB with about 494 GiB free.
