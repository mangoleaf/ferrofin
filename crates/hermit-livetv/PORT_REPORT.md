# Port Report — `hermit-livetv`

Wave 5 PortJob. Live TV is a **deferred** Hermit subsystem (see `brain/DEFERRED.md`,
`brain/PLAN_HERMIT_PORT.md`).

## What was implemented

`DisabledLiveTvManager` — the no-op implementation of the `hermit-traits`
`LiveTvManager` trait. It mirrors upstream Jellyfin's behaviour when no Live TV
services are configured: no tuners, no guide/EPG data, no recordings. Every
method returns the empty/disabled state and never errors.

- `#[derive(Debug, Default, Clone, Copy)]` zero-sized unit struct.
- `get_live_tv_info` → `LiveTvInfo::default()` (`is_enabled = false`, no
  services, no enabled users).
- `get_programs` → `QueryResult::default()` (empty items, `total_record_count = 0`);
  the query and options args are intentionally ignored (no guide data).
- `reset_tuner` → `Ok(())` no-op (no tuners to reset).

## Traits satisfied

Implements `hermit_traits::stubs::LiveTvManager` (defined in
`crates/hermit-traits/src/stubs/live_tv.rs`), a minimal deferred-slice port of
`MediaBrowser.Controller.LiveTv.ILiveTvManager`. All three trait methods are
implemented:

| Trait method         | Impl behaviour                          |
|----------------------|-----------------------------------------|
| `get_live_tv_info`   | disabled `LiveTvInfo`                    |
| `get_programs`       | empty `QueryResult<BaseItemDto>`         |
| `reset_tuner`        | `Ok(())` no-op                           |

Object safety is exercised: the `coerces_to_dyn_manager` test coerces the struct
to `Arc<dyn LiveTvManager>` and drives it through the trait object.

## Tests

4 unit tests (`crates/hermit-livetv/src/lib.rs`), all passing:

- `info_is_disabled` — asserts `!is_enabled`, empty services, empty enabled_users.
- `programs_is_empty` — asserts empty items and `total_record_count == 0`.
- `reset_tuner_is_noop_ok` — asserts `Ok(())`.
- `coerces_to_dyn_manager` — asserts trait-object coercion + dispatch.

The tests use a hand-rolled single-poll `block_on` executor (no async runtime
dependency): the manager's futures resolve on first poll, so no tokio/dev-dep is
pulled in.

## Coverage

`cargo llvm-cov nextest -p hermit-livetv --fail-under-lines 80 --summary-only`

- **Line coverage: 89.74%** (35/39 lines), gate floor 80% — **PASS**.
- Regions 91.89%, functions 75.00%.
- 4 uncovered lines: the `RawWakerVTable` no-op closures
  (`clone`/`wake`/`wake_by_ref`/`drop`) inside the test-harness `block_on`. They
  are never invoked because `DisabledLiveTvManager` futures are `Ready` on the
  first poll, so the waker is never cloned or woken. This is **test scaffolding,
  not production code** — every production line and branch is covered.

## Parity vs xUnit (upstream Jellyfin)

Upstream Jellyfin has **no xUnit test suite for a disabled/no-op Live TV manager**
— the real `LiveTvManager` is a large stateful service (tuners, timers,
recordings, series-timers, `ILiveTvService` backends) and its tests exercise that
machinery, none of which is ported here. There is therefore no upstream test to
port line-for-line. The 4 tests written are Rust-native contract tests for the
deferred stub's invariant: "Live TV always reports disabled/empty and never
errors." Parity is at the **behavioural** level (matches Jellyfin's
no-services-configured state), not test-for-test.

## Deferrals

- Full `ILiveTvManager` surface: timers, series timers, recordings, channels,
  tuner discovery/registration, `ILiveTvService` per-backend strategy interface.
- Real tuner/EPG port (guide ingestion, program scheduling) — future work.
- No async-runtime integration test (unnecessary: futures are synchronous-ready).

## Gate summary

| Check                                                                 | Result |
|-----------------------------------------------------------------------|--------|
| `cargo fmt --all`                                                     | clean  |
| `cargo fmt --all --check`                                            | clean  |
| `cargo build --workspace`                                            | pass   |
| `cargo clippy --all-targets --all-features -- -D warnings`          | clean  |
| `cargo test -p hermit-livetv`                                        | 4/4 pass |
| `cargo llvm-cov nextest -p hermit-livetv --fail-under-lines 80`      | 89.74% — pass |

### Honest note on `cargo test --workspace`

The **workspace-wide** test run does **not** pass end-to-end, due to a failure in
a *different* Wave-5 sibling crate, **not** `hermit-livetv`:

- `hermit-providers` has a failing **doctest**: `unresolved import
  hermit_model::entities::MetadataProvider` (a stale doc example referencing a
  path that no longer exists). All of hermit-providers' real unit/integration
  tests pass; only the doctest aborts.

`hermit-livetv`'s own build, clippy, tests, and coverage are all green. The
workspace test failure is pre-existing and unrelated to this crate; it should be
tracked against `hermit-providers`.
