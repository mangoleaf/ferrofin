# ST03: a trigger's maximum runtime applies to the runs it starts

`MaxRuntimeTicks` was enforced as the smallest limit across **all** of a
task's triggers, for any running invocation — including one an administrator
started by hand. Two triggers with different limits could not behave
independently, and a manual run was killed by a limit it never agreed to.

Source oracle: Jellyfin `4910aafa1a`, `ScheduledTaskWorker.GetTrigger`
(`TaskOptions { MaxRuntimeTicks = info.MaxRuntimeTicks }` per trigger),
`ExecuteInternal` (`CancelAfter(options.MaxRuntimeTicks)` when options carry
one) and `TaskManager.QueueScheduledTask`/`ExecuteQueuedTasks`, whose queue
entries carry their options. A manual start passes no options.

## Behavior

- The run records the limit of the trigger that started it: a daily, weekly or
  interval trigger that fires, or a startup trigger (including a late
  registration's). A manual run, the API's start, and a trigger without a limit
  record none.
- The scheduler sweep aborts a run only when it has exceeded **its own**
  limit, measured from when it was claimed; the minimum across triggers is
  gone.
- A request queued while the task is busy keeps its trigger's limit through the
  queue and starts with it (`ExecuteQueuedTasks`); the first request per task
  wins, as before.
- The limit is cleared when a run ends or is stopped, so it never carries to
  the next invocation.

## Verification

- Unit: a trigger-started run past its limit is aborted; a manual run of a
  task whose trigger has a limit survives a sweep; a request queued behind a
  running task starts with its own limit and is aborted by it; a run an
  unlimited trigger starts survives a sweep even though another trigger has a
  limit.
- Production binary, previous baseline vs updated, a manual "Scan Media
  Library" over 12,000 files with a daily trigger of `MaxRuntimeTicks = 1`
  configured (the trigger itself never fires during the test):

  | | result | time |
  |---|---|---|
  | before | **Aborted** by the first sweep | 29 s |
  | after | **Completed** | 61 s |

## Limitations

- When several triggers of one task are due in the same sweep, the first one's
  limit is used; upstream queues one run per fired trigger and the task manager
  keeps the first queued request, which is the same outcome.
- The limit is checked by the 30-second sweep, so an abort can lag the limit by
  up to one period; this predates the change.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,447, extensions 96, livetv 296, wasm
62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. The first Clippy run flagged a `clone` on a `Copy` type in the new
test; it was fixed and Clippy and the scheduler tests rerun clean, while the
package and coverage results were taken just before. Line coverage over core's
own sources: **95.10%**, above the 80% gate; traits and server composition are
coverage-exempt. The instrumented build took `target` to 42.9 GiB, above the
30 GiB cap; its cache was removed right after the numbers were taken, leaving
24.2 GiB with about 494 GiB free.
