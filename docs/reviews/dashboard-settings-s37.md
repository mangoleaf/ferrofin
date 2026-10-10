# S37 — indexed image uploads

Jellyfin's indexed upload route accepts the URL index but passes `null` to
`ProviderManager.SaveImage`. Backdrops therefore append even when the URL names
an existing slot; single images replace their only slot. Ferrofin passed the
index through, replacing backdrops and allowing arbitrary slot selection.

The route now saves without an index, retaining the existing `i32` URL binding,
admin authorization, item lookup, content-type and base64 validation. Internal
indexed saves and indexed deletion remain available to their existing callers.
Primary source: `4910aafa1a`, `Jellyfin.Api/Controllers/ImageController.cs`,
`SetItemImageByIndex`; the full controller capture is retained under
`/tmp/ferrofin-s37/ImageController.cs`.

Handler regressions capture the provider arguments for Backdrop and Primary,
including minimum/maximum integers, negative values and existing slots. Invalid
and overflowing indices return 400 without saving. The production HTTP fixture
checks append ordering and surviving files. N04's extrathumbs and L17's destination fixtures now
normalize the starting backdrop count through real DELETE requests before each
configuration phase, so it tests the requested destination with append semantics.

The native oracle uses distinct PNG colors and checks catalog paths and served
body hashes, comparing the previous production binary, Jellyfin 12.1.0 and the
updated production binary. It exercises existing, zero, negative and maximum
indices, invalid binding, single-image replacement and preserved backdrop order.
Screenshot multiplicity (S39) and mutation lifecycle (S40) remain separate open
findings.

The first dashboard HTTP attempt exposed L17's older replacement assumption;
its failed log remains in `/tmp/ferrofin-s37/s37/http-001.log`. Controlling the
initial catalog corrected the fixture, and the complete regression suite passed.
The server matrix regenerated the executable after the initial native check;
the final native check was rerun against the preserved binary. That matching
check, source hashes and profile hashes passed the final evidence audit.

Formatting, strict workspace Clippy, workspace doctests, the SQL boundary,
production build, handler regressions, the real HTTP integration fixture and
all three native checks passed. The complete source-qualified workspace matrix
passed **8,412 tests** across 21 packages and
201 default targets. API and server were rerun; all other
packages retain source-identical independent results from the preceding passing
matrix, verified across all dependency kinds. Real FFmpeg tests were enabled;
five existing DB/provider skips remain visible and WASM guest tests were
separately disabled explicitly.

Fresh unseeded **ferrofin-api line coverage: 86.45%**, above the
separate 80% gate. Server composition is coverage-exempt under CLAUDE.md.

Ten identical `Primary/0` PNG uploads per production binary yielded median local
request times of **9.039 ms before** and
**8.176 ms after** (Jellyfin: 22.512 ms).
These are bounded observations on a shared host, not a controlled benchmark;
primary replacement keeps the timing workload constant while distinct-color
backdrop checks prove the changed behavior separately.

Logs, exact source/binary/profile hashes and retained native results are indexed
in `/tmp/ferrofin-s37/quality-proof.json`. Only generated build caches were
cleaned; the worktree, commits, production binary, native datasets and profiles
remain. Validation checkpoint: target **12.84 GiB**, host free
**509.79 GiB**. Baseline advancement follows the separate commit.
