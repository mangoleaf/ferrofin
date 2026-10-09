# L32: trickplay storage beside media

Generation, pruning and tile lookup now use the owning library's live
`SaveTrickplayWithMedia`. A failed write at the selected location does not
fall back to internal storage. Lookup switches immediately after a setting
change; existing files remain at their old location until migration runs.

The migration task reads live ownership, extraction enablement and destination.
It moves existing tiles in either direction without encoding or changing their
bytes. It includes owned physical videos and complete channel VOD, since the
declared SourceTypes field is inert in the pinned query. Nonvideo, virtual and
folder rows are excluded. Extraction disabled means no migration.

Source migration quirks are preserved: an existing empty destination prevents
movement, matching files in a populated destination are overwritten, unrelated
destination files remain, and directory enumeration/cleanup errors are reported.
Source-container removal is nonrecursive; an ordinary remaining file can cause
an error after tiles have moved. No unrelated file is deleted to hide that error.

Sidecar paths retain the full dotted basename. Leading-dot filenames also
match `Path.ChangeExtension`, including `.mkv` becoming `.trickplay`; the
internal GUID layout is unchanged.

Pinned source oracle: Jellyfin `4910aafa1a`,
`Emby.Server.Implementations/Library/PathManager.cs`,
`Jellyfin.Server.Implementations/Trickplay/TrickplayManager.cs`, and the
migration scheduled task. Native Jellyfin 12.1.0 is separate runtime evidence.

The shared HTTP fixture now uses the actual internal data directory. Its
ordinary-error test also removes the injected database error and repeats the
same refresh, requiring successful disabled cleanup as a positive control.
This revalidates L31's error behavior with a fixture that actually reaches
discovery. Migration's seeded video rows now include the normal MediaType.

Validation passed: 47 focused core tests, four real-server HTTP tests, the
trait build and SQL boundary, formatting and strict workspace Clippy across
all targets/features. Fresh L32 execution merged with the verified L31 full
profile covers 110,397/116,301 core lines (94.92%); 22 current binaries and
98 fresh profiles were exported, with no LLVM diagnostics.

Native before/after used the same fixture and settings sequence. Jellyfin
12.1.0 independently passed the six destination/catalog checks. All migration
phases retain the original tile hash. Measured completion times in milliseconds:

| Phase | Before | After | Jellyfin 12.1 |
|---|---:|---:|---:|
| generate internal dotted name | 107.47 | 108.37 | 167.56 |
| migrate to sidecar | 53.11 | 53.18 | 58.4 |
| migrate back internal | 53.92 | 53.4 | 25.13 |
| disabled migration retains internal | 52.5 | 55.79 | 18.31 |
| reenabled migrates sidecar | 52.21 | 53.37 | 18.38 |
| blocked sidecar has no internal fallback | 53.03 | 69.86 | 77.77 |

These are shared-host observations with polling overhead, not a general
performance claim. Before migration chose the wrong destination and the blocked
sidecar operation incorrectly reused internal storage; after/reference match
the expected files and responses.

Evidence: `/tmp/ferrofin-dashboard-l32-checks.json`,
`/tmp/ferrofin-dashboard-l32-coverage.json`, and the three
`/tmp/ferrofin-dashboard-l32-native-*.json` records. The instrumented target and
normal target together use 23.79 GiB, with 671.44 GiB free. All owned native
children exited. Source, worktree and commits remain preserved.
