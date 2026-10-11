# ST04: a trigger save that cannot be written is an error

Saving a scheduled task's triggers changed the in-memory schedule and then
wrote the store; a failed write was only logged, so the API answered success
and the schedule vanished at the next restart.

Source oracle: Jellyfin `4910aafa1a`, `ScheduledTaskWorker.Triggers` (setter)
and `SaveTriggers`: the file is written first and the in-memory triggers are
assigned afterwards, and an I/O failure propagates to the caller.

## Behavior

- `set_triggers` validates the triggers, writes the store, and only then
  replaces the task's schedule and the stored overrides. A failed write returns
  a backend error (HTTP 500 from `POST /ScheduledTasks/{id}/Triggers`) and
  leaves both the running schedule and the stored map as they were.
- An unknown task is still reported as not found, before anything is written.
- The run-history store keeps its log-only behavior: upstream's result save is
  part of finishing a run, and a history write must not fail the run it
  describes.

## Verification

- Unit: a store whose directory cannot exist (a file stands in its place)
  makes `set_triggers` fail with "could not save the triggers", leaving the task's
  triggers and the stored overrides empty; the existing persistence, restart and
  invalid-trigger tests still pass.
- Production binary, previous baseline vs updated, config directory made
  read-only and a trigger saved:

  | | HTTP status | triggers afterwards | store file |
  |---|---|---|---|
  | before | 204 | the unsaved interval, until restart | none |
  | after | **500** | unchanged (the default) | none |

## Limitations

- The write is still atomic-by-rename rather than upstream's truncating
  `FileMode.Create`; a failure partway leaves the previous file intact, which is
  safer, and the visible result (an error) is the same.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on core passed: core 2,446, extensions 96, livetv 296, wasm
62, server 230. Real FFmpeg tests enabled; the WASM guest build remains
disabled. Line coverage over core's own sources: **95.10%**, above the 80% gate;
traits and server composition are coverage-exempt. The instrumented build took
`target` to 42.9 GiB, above the 30 GiB cap; its cache was removed right after
the numbers were taken, leaving 24.2 GiB with about 494 GiB free.
