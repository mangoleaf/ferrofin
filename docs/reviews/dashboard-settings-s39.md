# S39 — screenshot upload replacement

Screenshots are single images in pinned Jellyfin `4910aafa1a`:
`BaseItem.AllowsMultipleImages` permits only Backdrop and Chapter. Ferrofin's
manual upload manager and artwork row writer treated Screenshot as appendable,
and its DTO image-tag loop incorrectly excluded screenshots.

Screenshot uploads now replace their single persisted image, use the unnumbered
`screenshot` filename, and emit Screenshot in ImageTags and blur hashes.
Internal indexed artwork writes also replace screenshots. Indexed HTTP upload
continues to ignore its route index, as completed in S37. Screenshot swap
rejection was already implemented and is verified alongside a backdrop control.

A real database regression checks explicit internal screenshot indices,
including usize::MAX; an existing DTO integration test now checks screenshot
tags and blur hashes. Filename regressions use the pinned single-image policy.
The dashboard HTTP fixture checks unindexed and indexed screenshot replacement,
stable filenames, existing files, DTO tags and swap rejection. Its S14 multiple
upload/deletion scenario now uses Backdrop; S14's database test still verifies
deleting individual slots from preexisting duplicate screenshot rows.

Native before/Jellyfin/after checks upload distinct PNG colors and verify counts,
paths and served image hashes. Source captures and raw checks are retained in
`/tmp/ferrofin-s39/`. Broader replacement cleanup and mutation lifecycle remain
S36/S40; this finding does not close those separate reviews.

Formatting, strict workspace Clippy, workspace doctests, the SQL boundary,
production build, targeted regressions, the real HTTP integration fixture and
all three native checks passed. The complete source-qualified workspace matrix
passed **8,413 tests** across 21 packages and
201 default targets. Providers, core and every dependent were rerun; all other
packages retain source-identical independent results from the preceding passing
matrix, verified across all dependency kinds. The closure includes Live TV,
extensions and WASM through their normal/dev dependencies; API tests remain
independent through the traits seam. Real FFmpeg tests were enabled;
five existing DB/provider skips remain visible and WASM guest tests were
separately disabled explicitly.

Fresh unseeded line coverage: **providers 91.98%**,
**core 95.40%**, each above its separate 80% gate. Server composition is coverage-exempt under CLAUDE.md.

Ten identical unindexed Screenshot PNG uploads per production binary yielded median local
request times of **8.564 ms before** and
**8.191 ms after** (Jellyfin: 16.966 ms).
These are bounded observations on a shared host, not a controlled benchmark;
the previous implementation grows its catalog while the corrected one replaces
a single image. Distinct-color delivery assertions prove correctness separately.

Logs, exact source/binary/profile hashes and retained native results are indexed
in `/tmp/ferrofin-s39/quality-proof.json`. Generated test executables and the coverage build cache were cleaned.
The small ordinary compiler cache is retained under the 30 GiB cap for subsequent
findings; the worktree, commits, production binary, native datasets and profiles
remain. Validation checkpoint: target **20.61 GiB**, host free
**507.43 GiB**. Baseline advancement follows the separate commit.
