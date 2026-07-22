# Port Report — `hermit-keyframes`

Port of Jellyfin's `Jellyfin.MediaEncoding.Keyframes` (C#) to Rust.

- **C# source:** `src/Jellyfin.MediaEncoding.Keyframes`
- **C# tests:** `tests/Jellyfin.MediaEncoding.Keyframes.Tests`
- **Rust crate:** `crates/hermit-keyframes`

## Scope

The C# project has three extractor families: **FfProbe**, **FfTool**, and **Matroska**
(EBML reader, constants, models). This wave ports the FfProbe + FfTool + KeyframeData
surface. The **Matroska** extractor is deliberately deferred (Wave 5, per
`brain/PLAN_HERMIT_PORT.md`) and is out of scope for this report. It has **no** upstream
xUnit tests, so its deferral does not affect the parity denominator.

## LOC estimate

| | C# (in scope) | Rust (out) |
|---|---|---|
| Source | ~180 LOC (FfProbe 133, FfTool 17, KeyframeData 30) | ~364 LOC (incl. inline unit tests) |
| Tests | ~28 LOC (1 file) | ~30 LOC (integration) |

Rust source is larger mainly because of the inline `#[cfg(test)]` unit tests, doc
comments, and an explicit `error.rs` (`KeyframesError`) that C# handles with raw
exceptions.

## Modules

| Rust module | C# origin |
|---|---|
| `keyframe_data` | `KeyframeData.cs` (root namespace) |
| `ff_probe` | `FfProbe/FfProbeKeyframeExtractor.cs` |
| `ff_tool` | `FfTool/FfToolKeyframeExtractor.cs` (upstream `throw new NotImplementedException()` → Rust `unimplemented!()`) |
| `error` | (no C# analogue — Rust error type for the spawn/IO path) |
| _(deferred)_ `matroska` | `Matroska/*` — Wave 5 |

## Test parity

Upstream C# xUnit inventory (whole test project):

- `FfProbeKeyframeExtractorTests.ParseStream_Valid_Success` — 1 `[Theory]` × 2 `[InlineData]` = **2 cases**
- No FfTool tests, no Matroska tests upstream.

**Total C# xUnit cases: 2. Faithfully ported and passing: 2.**

Ported as `tests/ff_probe_keyframe_extractor_tests.rs` — `#[rstest]` + 2 `#[case]`,
mirroring the `[Theory]`/`[InlineData]` structure 1:1:

| Case | Data fixture | Oracle fixture | Assertions |
|---|---|---|---|
| 1 | `keyframes.txt` | `keyframes_result.json` | `total_duration` + `keyframe_ticks` |
| 2 | `keyframes_streamduration.txt` | `keyframes_streamduration_result.json` | `total_duration` + `keyframe_ticks` |

The Rust test preserves the C# assertions exactly (`Assert.Equal(expected.TotalDuration,
result.TotalDuration)` and `Assert.Equal(expected.KeyframeTicks, result.KeyframeTicks)` →
`assert_eq!` on `total_duration` and `keyframe_ticks`). No assertion was weakened, dropped,
or loosened.

### Fixture fidelity

All four test-data files are **byte-identical** to the C# originals (`diff -q` clean):
`keyframes.txt`, `keyframes_result.json`, `keyframes_streamduration.txt`,
`keyframes_streamduration_result.json`. The result JSONs are consumed as the oracle via
`serde_json` with `#[serde(rename_all = "PascalCase")]` mapping `TotalDuration` /
`KeyframeTicks` to the Rust fields.

### Spot-checked oracle values

- **Case 1:** fixture has `stream,N/A` (unparseable → 0) and `format,706.336000`. Stream ≤ 0
  → format wins → 706.336 s → 706336 ms → **7063360000 ticks** = expected `TotalDuration`. ✓
- **Case 2:** fixture has `stream,100` and `format,101`. Stream (100) > 0 → **stream wins**
  over format → 100 s → **1000000000 ticks** = expected `TotalDuration`, with the shorter
  keyframe list. ✓ (exercises the "prefer stream duration" branch)

### Beyond-parity unit tests (Rust-only, not required for parity)

`ff_probe.rs` adds 9 inline unit tests covering branches the 2 C# integration cases don't
isolate: empty/malformed-line skipping, non-`K_` flags, unparseable pts/duration, format
fallback, case-insensitive line types, banker's-rounding (`Convert.ToInt64` semantics),
millisecond rounding (`TimeSpan.FromSeconds`), and the real spawn/read/wait path via a fake
POSIX ffprobe script + a missing-binary error case. These raise line coverage without
inflating the parity count.

## Semantic parity notes

- `Convert.ToInt64(double)` → `f64::round_ties_even() as i64` (round-half-to-even /
  banker's rounding). Matches .NET.
- `TimeSpan.FromSeconds(x).Ticks` → rounds seconds to the nearest **millisecond**, then
  ×10_000 ticks. This matches modern .NET (Core 3.0+) behaviour, which is what Jellyfin
  targets. Both parity fixtures resolve to whole-millisecond durations, so this rounding
  boundary is covered by the dedicated `time_span_from_seconds_rounds_to_millisecond` unit
  test rather than by the integration fixtures.
- ffprobe argument vector is preserved verbatim.
- `StringComparison.OrdinalIgnoreCase` → `eq_ignore_ascii_case` (line-type keywords are
  ASCII, so equivalent).

## Deviations / deferrals

- **Matroska extractor deferred** to Wave 5 (no upstream tests; excluded from parity
  denominator).
- **FfTool** is an upstream `NotImplementedException` stub, ported as `unimplemented!()`;
  its test just asserts the panic. No semantic loss.
- **Process-spawn path** (`get_keyframe_data`): the C# `GetKeyframeData` sets
  `ProcessPriorityClass.BelowNormal` and does a kill-on-exception dance; the Rust port
  spawns, pipes stdout, parses, and reaps via `wait()`. Priority-lowering is intentionally
  omitted (best-effort, no-logger, non-semantic in C#). The parse logic — the sole
  testable behaviour — is identical.

## Verification (this run)

- `cargo test -p hermit-keyframes` → **11 passed** (2 integration parity cases + 9 unit),
  0 failed, 0 ignored.
- `cargo clippy -p hermit-keyframes --all-targets -- -D warnings` → **clean**.

## Coverage

- Line coverage **99.4%** (1 round, gate passed).
- Sole uncovered line: `ff_probe.rs:59` — the `ok_or_else` closure for
  `process.stdout.take() == None`, unreachable given `Stdio::piped()`; would require mocking
  `std::process::Child`.

## Parity score

**2 / 2 = 1.00** — every C# xUnit case is faithfully ported with identical fixtures and
unweakened assertions. No cases skipped.
