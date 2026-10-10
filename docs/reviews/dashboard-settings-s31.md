# S31: busy scheduled-task queue and completion context

A request to run a task that is already running is remembered and started when
the current run ends, instead of being dropped.

Source oracle: Jellyfin `4910aafa1a`,
`Emby.Server.Implementations/ScheduledTasks/TaskManager.cs`
(`QueueScheduledTask`, `OnTaskCompleted`, `ExecuteQueuedTasks`) and
`ScheduledTaskWorker` (triggers call `QueueScheduledTask`).

## Behavior

- **Queue producer.** `FerrofinTaskManager::queue_scheduled` starts an idle
  task, and for a busy one appends `(task, trigger)` to an ordered queue. The
  scheduler uses it for startup triggers and for every due trigger, including
  those of a task that is currently running; previously the sweep skipped
  running tasks' triggers entirely and a startup trigger on a busy task was
  logged and lost.
- **Drain.** Wherever a run completes — normal finish, failure, or cancel/
  abort (`record_result` and `stop_run`) — the queue is drained after the task
  is `Idle`: every request is taken, the first per task is kept
  (`list.All(i => i.Item1 != item.Item1)`), and each task that is idle is
  started. Request and drain take the same lock, and the producer checks state
  and pushes under it while the drain runs after the state flips, so a request
  is started directly or seen by the drain — never lost between them.
- **Context.** A drained run starts inside the completing run's tokio task, so
  it inherits that worker's culture scope, as upstream's inherits the
  completing worker's execution context, not the producer's.
- **`StartTask` is unchanged.** The HTTP start path stays upstream's
  `Execute`, which refuses a running task (`400` here).

## Verification

- Unit: three requests for a busy task coalesce into one follow-up run that
  starts only after the first ends; an unknown task is `NotFound`; the
  explicit start path still refuses a running task; cancelling a busy task
  starts its queued request; a due interval trigger found by the sweep on a
  running task queues a follow-up; a request queued under culture `de` starts
  under the completing run's `sv`.
- Production binary, previous baseline vs updated, 6,000-folder library so the
  scan runs about 28 s, interval trigger of one second on the library scan
  (the scheduler sweeps every 30 s):

  | | idle `StartTask` | `StartTask` while running | follow-up gaps (run end → next start) |
  |---|---|---|---|
  | before | 204 | 400 | 0.02 s, **30.56 s** |
  | after | 204 | 400 | 0.03 s, **1.94 s**; a second run: 0.21 s, 30.47 s |

  In the first updated run a sweep landed during the scan, the trigger was
  queued and the follow-up started as soon as the scan ended; in the second a
  sweep happened not to fall inside the run, matching the baseline. The
  observation depends on where the 30 s sweep falls relative to the run, so it
  is supporting evidence; the unit tests are the deterministic check.
  A single request costs one lock and a vector push; no latency was measured.

## Limitations

- Library scans requested through `LibraryManager` use their own scan queue
  and are not routed through this registry, so they are unaffected.
- Task options are limited to the trigger tag; Ferrofin tasks take no other
  per-run options.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,427, extensions 96, livetv 296,
wasm 62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. Line coverage over core's own sources: **95.08%**, above the 80%
gate (the traits crate and server composition are coverage-exempt). The
instrumented build took `target` to 39.95 GiB, above the 30 GiB cap; its cache
was removed right after the numbers were taken, leaving 21.5 GiB with about
495 GiB free.
