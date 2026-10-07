# C01: typed dashboard configuration documents

Reviewed and implemented on 2026-10-07, based on main
`782ca9f28cc938bf15b5a55b425c435673d2f392`.

Older partial named configurations could make dashboard fields disappear or show
the wrong state. Malformed stored values were handled differently by section:
encoding returned an error while networking echoed the unusable value.

Core named configuration reads now deserialize through their DTOs, supply defaults
for omitted members, and report malformed documents before returning defaults.
Reading never rewrites the stored file. The existing network aliases, Live TV
tuner/provider projection, non-finite encoding response checks, and plugin-owned
JSON documents retain their behavior.

Main already contained typed named-configuration writes when this worktree was
created. This change completes the typed read path. It closes the transport/default
handling portion of C01; it does not claim that every persisted setting has a runtime
consumer. Store validation (C02), update propagation (C03), DTO coverage (C04), and
the individual settings findings remain separate work.

The reference behavior is Jellyfin v12.0-rc7 (`4910aafa1a`):
`ConfigurationController.UpdateNamedConfiguration` deserializes to the store's
type, and `BaseConfigurationManager.LoadConfiguration` logs deserialization
failures and constructs the type's default instance. Ferrofin keeps its JSON
storage and existing handling of unreadable files.

## Verification

All 17 tests in `ferrofin-api::config` pass. Regression coverage includes partial
documents for encoding, networking, metadata, NFO, and Live TV; malformed stored
values; canonical network aliases; case-insensitive section names; preservation
of original files; and plugin-owned document shapes. Existing tests also cover
typed writes, rejected writes preserving previous content, syntax/read failures,
and the non-finite-number serialization contract.

Real HTTP checks ran both binaries on loopback against the same disposable data
directory. The fixture contains one administrator and no media. Each probe restored
its original file after reading it. Both binaries also rejected malformed writes
without replacing the previous good document, and round-tripped a plugin-owned
document.

| Stored document / requested field | Before | After |
|---|---|---|
| Metadata `{}` / `UseFileCreationTimeForDateAdded` | 200, field absent | 200, `true` |
| NFO with only `UserId` / `SaveImagePathsInNfo` | 200, field absent | 200, `true` |
| Live TV with only `PrePaddingSeconds` / `SaveRecordingNFO` | 200, field absent | 200, `true` |
| Encoding with a string in `EnableThrottling` | 500 | 200, default `false`, warning |
| Networking with a nonnumeric `InternalHttpPort` | 200, unusable string echoed | 200, default `8096`, warning |

All five stored files remained byte-for-byte unchanged by GET.

Repository gates passed:

- `cargo fmt --all --check`.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- `cargo nextest run --workspace`: 7,722 passed, 5 skipped.
- `cargo test --workspace --doc`.
- `cargo llvm-cov nextest -p ferrofin-api --fail-under-lines 80 --summary-only`,
  with the contributor guide's filename exclusion for other crates/apps:
  871 tests passed; **87.04% API line coverage**.

Cargo builds used `--offline`, two build jobs, and four nextest test threads.
The live checks cover configuration HTTP behavior; no GPU pipelines were exercised.

## Before/after timings

Authenticated `curl` GETs of `/System/Configuration/{key}`, dev builds of the
baseline and changed server, same host and data directory. Each endpoint received
20 warm-up requests followed by 100 measured requests; `curl`'s `time_total`
includes the loopback connection. Other builds were running, so these numbers
are a local regression check, not a stable performance comparison.

| Section | Median before / after (ms) | p95 before / after (ms) |
|---|---|---|
| Metadata | 1.014 / 1.069 | 2.060 / 1.624 |
| Networking | 1.441 / 1.168 | 3.181 / 1.829 |
| Live TV | 1.549 / 1.484 | 4.337 / 2.330 |
| Encoding | 1.125 / 1.186 | 2.140 / 1.993 |
