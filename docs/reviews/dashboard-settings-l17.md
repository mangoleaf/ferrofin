# L17: artwork saved beside media

`SaveLocalMetadata` now controls the shared artwork destination writer used by
manual uploads, provider-manager downloads, scanner artwork and music refreshes.
The writer resolves the owning library's current options, including physical
location IDs, and reads the current `ImageSavingConvention`. Existing local
artwork is retained; newly acquired managed images use the pinned upstream
media-adjacent filenames. Disabling the option keeps acquisitions internal.

The Legacy/Compatible policy covers mixed folders, movies, music videos,
episodes, physical/virtual seasons, artist/album folders, DVD/Blu-ray folders
and the individual artwork types. Tracks, photos, extras, channel items and
non-primary episode images remain internal. Compatible extra backdrops use
`extrafanart`; the named NFO option enables their `extrathumbs` copies.

Writes use complete staged files and atomic replacement. Adjacent artwork honors
the process umask for new files and preserves existing file permissions while
clearing the read-only owner flag, so shared readers retain access. Single-output media
write failures retain the internal image, while duplicate-output failures are
reported. Replacing an image removes its previous eligible file when the
filename or extension changes. Shared album-cover assets remain owned by all
of their references. Album inheritance publishes the album's selected local
cover while tracks retain their internal shared cover. Identical destination
bytes preserve the file's modification time on unchanged rescans. Indexed
backdrop uploads preserve the other images rather than replacing the whole type.
Image reads and swaps use insertion order, and an indexed replacement updates
its existing row so its identity and slot stay stable. This prevents random UUID
sorting or delete/reinsert from moving the image being addressed.

Indexed deletion remains a separate open work item, **S14**: the existing delete
operation ignores the requested slot and removes the whole image type. This
finding establishes destinations and indexed saving, not deletion parity.

Sources: Jellyfin `4910aafa1a`, `ImageSaver.SaveImage`, `GetSavePaths`,
`GetStandardSavePath`, `GetCompatibleSavePaths`, `GetBackdropSaveFilename`, and
`BaseItem.SupportsLocalMetadata` / `Photo.SupportsLocalMetadata`. The native
reference is Jellyfin **12.1.0**, kept separate from the pinned source version.

Validation:

- Formatting, strict workspace Clippy, SQL-boundary checks and server build pass.
- 180 utility tests, 852 provider tests (4 environment skips), 490 focused core
  tests and all **2,218 full-core tests** and the real-server `dashboard_metadata_locale` regression pass. The
  HTTP scenario checks uploads, automatic downloads, live flags/convention,
  old-file removal, write failure, duplicated backdrops and indexed replacement.
- Native disposable-fixture checks use both pre-change `c7831614` and current
  binaries built with debug info and incremental compilation disabled, plus
  Jellyfin 12.1.0. All six after-change file/permission/count/order observations
  match the reference. Before-change files were always internal and the second
  backdrop replaced the first. After-change adjacent files are mode 0644 with
  the fixture's umask and both backdrop slots remain in upload order.
- Four settled Primary uploads measured median **7.36 ms before / 15.40 ms
  after**, ranges 7.20–10.46 / 11.71–26.76 ms. These are unisolated native
  observations on a busy host, including added file writes, not a benchmark
  improvement claim. Docker was unavailable for `bench/`.
- Fresh utility coverage is **96.13%**, provider coverage **91.88%**. A retained
  pre-cleanup core profile did not provide usable coverage with the reduced
  build settings. The complete fresh core run passes at **95.14%**, without LLVM
  export warnings. All three crates exceed their separate 80% line gates.
- The full-suite check exposed an outdated episode fixture: it counted
  `/episode/.../images` as episode metadata requests. Its route dispatch now
  distinguishes image-list requests; both normal regression tests pass without
  weakening their metadata request-count assertions. Strict workspace Clippy
  was rechecked after this fixture correction and passes.

Evidence is retained under `/tmp/ferrofin-dashboard-l17-`: `checks.json`,
`coverage.json`, `native-{before,after,reference}.json`, and
`bounded-{before,after,reference}.log`. A reused Cargo fingerprint after the
baseline snapshot produced a restore-build error; generated local-crate outputs
were cleaned and the current-source build then passed. This did not affect the
separately retained binaries used for the measurements.

The disk cleanup preserved source and history and removed only generated
`target/` artifacts. Subsequent validation uses one process at a time, no
incremental/debug output, a 30 GiB target budget, 512 GiB host free-space reserve
and pooled coverage profiles removed after export.
