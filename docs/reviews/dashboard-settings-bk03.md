# BK03: a failed restore is reported, not announced as a success

The web client treats "the server answers again" as a finished restore. A
scheduled restore that failed was logged and the boot carried on regardless —
over a data directory that might be half replaced — so the client announced
success for a restore that had not happened. The marker was removed either way
and nothing durable recorded the failure.

Source oracle: Jellyfin `4910aafa1a`, `Program.cs` (`RestoreBackupAsync` runs
before the host starts and its exceptions propagate out of startup, so the
server does not come up) and `BackupService.RestoreBackupAsync` (missing
archive, missing manifest and incompatible engine are thrown, not swallowed).

## Behavior

`apply_pending_restore` now returns a `RestoreFailure` that says whether
extraction had begun:

- **Rejected before anything was written** (the marker names no archive, the
  archive is missing, foreign or not restorable): nothing was touched, so the
  server boots on its existing data, logs the error, and writes an **error
  entry to the activity log** — "Restoring the backup failed", type
  `BackupRestoreFailed`, with the reason as its overview — which the dashboard's
  Activity page shows.
- **Failed during extraction** (a file cannot be written, the disk fills): the
  trees may be partly replaced. The boot is aborted with an error, as upstream's
  startup exception does, instead of serving them; the process exits and the
  client never sees the server come back.
- The pending marker is still removed in every case, so a failed restore
  cannot loop the next boot. Success is unchanged.

## Verification

- Unit: a missing archive and a foreign-engine archive are rejected with
  `partially_applied == false` and the archive named in the message; a valid
  archive whose extraction cannot write is `partially_applied == true` and still
  consumes the marker; a marker naming nothing is an untouched failure with no
  archive.
- Production binary, previous baseline vs updated, two scheduled restores
  started from a stopped server:

  | | marker names a missing archive | real archive, `system.json` read-only (fails mid-extraction) |
  |---|---|---|
  | before | boots; no activity entry | **boots and serves** |
  | after | boots; activity entry **"Restoring the backup failed"** | **process exits** |

## Limitations

- The activity entry is written once the application state is wired, so it
  appears after the boot completes, not during the restore.
- A part-way failure leaves whatever was already extracted in place; there is
  no rollback.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on api passed: api 933, server 231. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run flagged
`serve_once` at 126 lines; the restore handling moved into two helpers and
Clippy, the server tests and the live check were rerun clean, while the api
results and coverage were taken just before that refactor. Line coverage over
api's own sources: **87.88%**, above the 80% gate; traits and server
composition are coverage-exempt. The instrumented build was cleaned right after
the numbers were taken, leaving `target` at about 24 GiB with about 494 GiB
free.
