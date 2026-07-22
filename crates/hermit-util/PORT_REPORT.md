# hermit-util — Port Parity Report

Port of **Jellyfin.Extensions** (C#) → **hermit-util** (Rust).

- C# source: `/home/mango/dev/3rdparty/jellyfin/src/Jellyfin.Extensions`
- C# tests:  `/home/mango/dev/3rdparty/jellyfin/tests/Jellyfin.Extensions.Tests`
- Rust crate: `/home/mango/dev/hermit/crates/hermit-util`

## Scope

The **`Json/` subtree** of the C# project (17 `System.Text.Json` converters + their
~10 test files) is **deliberately out of scope** for this crate — those are
serializer-integration adapters with no counterpart in the Hermit Rust stack
(serde would be used instead). Only the 13 leaf-utility modules were ported. All
parity figures below are computed against the **non-Json** C# test surface.

## LOC estimate

| | Value |
|---|---|
| C# ported source (13 modules, excl. Json) | ~988 LOC |
| Rust source (incl. inline `#[cfg(test)]` modules) | ~1431 LOC (1081 impl + ~350 tests approx.) |

## Modules ported (13)

`copy_to_extensions`, `dictionary_extensions`, `enumerable_extensions`,
`error`, `file_helper`, `formatting_stream_writer`, `guid_extensions`,
`path_helper`, `read_only_list_extension`, `shuffle_extensions`,
`split_string_extensions`, `stream_extensions`, `string_builder_extensions`,
`string_extensions`.

## Test correspondence (spot-checked, faithful)

| C# test file | C# rows (xUnit-expanded) | Rust cases | Verdict |
|---|---|---|---|
| StringExtensionsTests | 38 | 38 | 1:1, same oracle values |
| PathHelperTests | 11 | 11 | 1:1 (incl. sibling-prefix security case) |
| CopyToExtensionsTests | 7 | 7 | 1:1 (2 valid + 5 OOB-error) |
| FileHelperTests | 1 | 1 | 1:1 |
| FormattingStreamWriterTests | 1 | 1 | 1:1 |
| ShuffleExtensionsTests | 1 | 1 | 1:1 |
| StreamExtensionsTests | 17 | 12 | 12/17 — see deferrals |
| **Total** | **76** | **71** | |

Spot-checks confirming assertion values match the C# oracle:
- `RemoveDiacritics` / `HasDiacritics`: all 12 diacritic rows (Kieślowski→Kieslowski,
  cœur→coeur, Korean identity, etc.) preserved exactly.
- `RightPart("Banana split.", '.')` → `""` (trailing-needle edge) — matches.
- `PathHelper.IsContainedIn` sibling-prefix collision (`/…/data` vs `/…/dataset`)
  asserts `false` in both — security-critical guard faithfully reproduced via
  trailing-separator comparison.
- `CopyTo` OOB rows (index −1, 6, empty dest, dest len 1, offset 1) all assert error.

Bonus Rust tests beyond the C# oracle (added coverage, not weakening): `trimmed`,
`truncate_at_null` (4), `get_clean_value` (5), `transliterated` (2),
`read_all_lines`, plus extra `FormattingStreamWriter` write/flush cases.

## Deviations & deferrals

1. **`a\ud800b` (lone UTF-16 surrogate) → `ab`.** A lone surrogate is
   unrepresentable in a Rust `&str`; the port substitutes `U+FFFD` (which its
   normalization strips) reaching the **same expected output**. Semantically
   faithful — both strip the invalid char. Not a weakening.

2. **5 stream rows not ported (3 C# test methods):**
   `IsStreamIdenticalAsync_MemoryStreamPairedWithSeekableNonMemoryStream` (×2),
   `…_NonMemoryStreamPairedWithMemoryStream_Swaps` (×2), and
   `…_BothSeekableNonMemoryStreams` (×1). These exercise .NET's `MemoryStream`
   `TryGetBuffer` **fast path** and the MemoryStream/non-MemoryStream **swap**
   branch — implementation details of the C# code. The Rust port has a single
   uniform `Read + Seek` path (no `MemoryStream` special-casing), so these
   branches **do not exist** to test. The observable contract they guard
   (seekable streams rewind to start before comparison) IS covered by the ported
   `…_BothSeekable_NonZeroPositions_SeeksToStart` and file-identical position
   tests. Legitimate semantic collapse, not a skipped/disabled upstream case.

3. **`publiclyVisible` `[true]/[false]` Theory pairs** (3 methods) collapse to one
   Rust case each — same reason (no `publiclyVisible` concept on `Cursor`). Both
   C# expected values are satisfied by the single Rust case.

4. **Async → sync.** C# APIs are `async Task`; the Rust port is synchronous
   `std::io` (tokio out of scope). No ported test depended on async behavior.

No test was weakened to pass. No upstream case was `// FIXME`-disabled.

## Parity score

- **Strict (every xUnit InlineData row counted):** 71 / 76 = **0.934**
- **Fair (excluding the 5 rows with no Rust analogue):** 71 / 71 = **1.00**

Reported parity: **0.934** (strict).

## Verification

- `cargo test -p hermit-util` → **101 passed; 0 failed; 0 ignored**.
- `cargo clippy -p hermit-util --all-targets -- -D warnings` → **clean**.
- Coverage (from coverage stage): **94.2 % line**, gate passed. Uncovered =
  defensive IO/seek error paths in `stream_extensions`, an iterator terminal
  branch in `split_string_extensions`, and lexical fallbacks in `path_helper`.
