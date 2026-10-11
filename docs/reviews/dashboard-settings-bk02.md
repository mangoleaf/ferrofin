# BK02: a restored backup's metadata follows its restored configuration

A scheduled restore is applied at the next boot, before the configuration
manager exists. The metadata destination was read from the **current**
`system.json` before the archive replaced it, so a backup whose configuration
named a different metadata root restored its files into the old root while the
recovered configuration pointed at the new one: after the restart the server
looked for its metadata where nothing had been restored.

Source oracle: Jellyfin `4910aafa1a`, `BackupService` (restore extracts the
archived `Config`/`Metadata`/… trees and then the server restarts into the
restored configuration). Jellyfin resolves `InternalMetadataPath` from the live
configuration at the time and so shares the mismatch; Ferrofin corrects it
rather than port it.

## Behavior

- The archive is extracted in two passes: everything except `metadata/` first
  (database, `config/`, trickplay, subtitles), then `metadata/`.
- Between the passes the restored `config/system.json` is read; if it names a
  `MetadataPath`, the metadata tree is extracted there (created if missing).
  Without one, the boot-time root is used as before.
- The boot hook and the restore share one reader for that setting
  (`restored_metadata_root`).

## Verification

- Unit: an archive whose `system.json` relocates the metadata, restored over
  boot roots that know nothing of it, puts `library/poster.jpg` in the relocated
  root — recreated when it no longer exists — and leaves no `metadata/` under
  the boot-time root. The existing round-trip, marker and manifest tests pass.
- Production binary, previous baseline vs updated: `MetadataPath` set to
  `meta1`, a marker file written there, a backup with metadata created, the
  setting changed to `meta2`, the backup restored, and the server restarted:

  | | `system.json` `MetadataPath` after restore | marker restored to `meta1` | marker restored to `meta2` |
  |---|---|---|---|
  | before | `meta1` | no | **yes** (the wrong root) |
  | after | `meta1` | **yes** | no |

## Limitations

- A relative `MetadataPath` is used as written (relative to the process), as
  the boot hook already did for the current configuration.
- Archives written by other Ferrofin versions restore the same way; only the
  destination of the metadata tree changed.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on api passed: api 932, server 231. Real FFmpeg tests
enabled; the WASM guest build remains disabled. Line coverage over api's own
sources: **87.84%**, above the 80% gate; traits and server composition are
coverage-exempt. The instrumented build was cleaned right after the numbers
were taken, leaving `target` at about 24 GiB with about 494 GiB free.
