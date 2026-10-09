# L27: registered media-segment providers

Library choices now come from actual registered producers. Saved order uses
exact display names, disabling uses .NET ordinal case-insensitive comparison,
and unranked providers retain stable intrinsic order. New provider namespaces
use the pinned MD5/UTF-16LE name identity. The core MediaSegments endpoint now
applies its default provider filter; Intro Skipper management reads its own
producer data independently, as upstream does.

Intro Skipper detection remains in its season task. The core extraction task
replays complete persistent output without repeating season fingerprinting.
Ordinary core create/delete changes served rows; actual producer editors,
detectors and authenticated WASM writes publish their snapshots before updating
that projection. Overwrite adopts legacy output from every registered producer,
including disabled ones, before clearing served rows. Item erasure invokes
cleanup; producer bulk erasure also handles cache-only items.

One retained adapter serializes snapshot publication. Extraction callbacks run
outside the mutation gate; revisions prevent stale extraction from overwriting
a newer edit or resurrecting bulk-erased output. Snapshot publication failure
preserves prior source and rows, while later projection failure leaves complete
source available for replay.

WASM scan targets describe inputs. Only actual loaded writers with completed
snapshot/identity evidence or their exact legacy namespace become segment
providers. State-only analyzers remain independent. Host-side policy skips stay
pending even if the library is re-enabled before the guest returns, and state
publication preserves concurrent guest keys. No new WIT role or setting is
introduced, and unloaded namespaces never become advertised providers.

Source oracles: Jellyfin `4910aafa1a`, MediaSegmentManager,
MediaSegmentsController and LibraryController; Intro Skipper
`db09359a520dc91cf51336e508634513d1800fa8`, SegmentProvider, Plugin,
SegmentEditorController and MediaSegmentEditorService.

This finding establishes provider selection, ordering and source retention.
Full Intro Skipper plugin/API parity, its independent authorization/query,
exclusion/refiner and analyzed-season behavior remain separate work. The plain
native Jellyfin runtime has no loaded Intro Skipper plugin, so plugin-reference
HTTP parity is unmeasured. The opt-in compiled example WASM guest is also a
separate check; actual inline component guests exercise the changed host paths.

The actual-manager fixtures use canonical stored GUIDs and isolated real media
paths. Canonical lookup exposed the old lowercase-ID fixture, while using `/`
exposed a separate shortcut-reader defect: it strips the root and loses library
Locations before setting consumers run. S20 records that ingestion/ownership
gap; this finding does not close it by implication.

Normal validation passes: 15 focused core tests, 33 API integration tests,
44 extension tests, 44 WASM tests, two traits media-segment regressions, the
SQL-boundary ratchet, real HTTP dashboard persistence, formatting, a fresh
server build and strict workspace/all-target/all-feature Clippy. The WASM
suite includes actual component instantiation and a guest write interrupted by
an off/on library policy change. Separate crate coverage is recorded below.

Native before/after uses isolated media, the loaded built-in Intro Skipper,
its actual advertised provider name in each build, 10 GET samples per saved
flag state, and the scheduled-task endpoint. Across off/on/off/on/off states,
the baseline serves both Intro and an unloaded WASM Outro regardless of the
flag. The changed build serves only registered Intro while enabled, zero while
disabled, and retains the management Intro throughout. Extraction completes
and retains only the registered producer's served output.

GET median milliseconds before: 3.363, 1.534, 1.560, 3.131, 1.583;
after: 2.270, 2.579, 2.223, 2.175, 2.129. Task wall time before/after:
26.775/27.269 ms, including a 25 ms polling interval. This is one unisolated
host observation; it establishes no general latency improvement. The new
GET path necessarily resolves owning-library/provider policy.

Evidence: `/tmp/ferrofin-dashboard-l27-checks.json`,
`/tmp/ferrofin-dashboard-l27-native-before.json` and
`/tmp/ferrofin-dashboard-l27-native-after.json`. Native script:
`/tmp/ferrofin-dashboard-l27-live.py`. The baseline's earlier fixture attempt
rejected its old `IntroSkipper` spelling before any policy observation; the
successful baseline uses that actual advertised name, while the changed build
requires `Intro Skipper`.

Fresh full core coverage passes: **95.23%**, 107,525 / 112,906 lines, all
2,284 tests passing across 22 instrumented binaries, no LLVM diagnostics.
The attempted L26 profile reuse measured only 2.35% against this build and
was rejected; its record remains at
`/tmp/ferrofin-dashboard-l27-incompatible-seed-coverage.json`. The passing
run uses no prior profile. 

Fresh full API coverage passes its numeric threshold: **86.08%**, 40,153 /
46,648 lines, all 893 tests passing across 54 instrumented binaries. This
seed-free export reports **53 functions with mismatched data**; the diagnostic
is retained in the coverage record and qualifies the measurement. Normal
integration and actual HTTP/native proof independently pass; this percentage
alone does not establish behavioral correctness.

Fresh full extension coverage passes: **94.22%**, 2,854 / 3,029 lines,
all 96 tests passing, no LLVM diagnostics. Fresh WASM coverage passes:
**85.20%**, 4,811 / 5,647 lines, all 61 selected host/runtime tests passing,
no LLVM diagnostics. The compiled example guest binary is built/listed but
its opt-in test is excluded; actual inline WAT guests run. Traits and server
are the documented coverage exemptions.

All four gates use fresh full suites with no prior profiles. Records and
merged profiles: `/tmp/ferrofin-dashboard-l27-coverage.json` and
`/tmp/ferrofin-dashboard-l27-ferrofin-<crate>.profdata`. Raw profiles were
removed after successful exports. Stale generated target cleanup removed
7.39 GiB of artifacts; after coverage, target is about 25.6 GiB and host
available space about 682 GiB. Source/worktree/commits and the baseline binary
remain intact. Full workspace tests/doctests remain the batch completion gate.
