# Port Report — `hermit-health`

Stage: **INTEGRATE**

Lean health-check router for Hermit — a local replacement for rest's `mlstudios-health`.
Provides `/livez` and `/readyz` probes backed by pluggable async `Checker`s, plus a
`utoipa` OpenAPI surface for the probe paths.

## Gate results

All commands were run from the workspace root (`/home/mango/dev/hermit`).

| Command | Result |
| --- | --- |
| `cargo fmt --all` | PASS (no changes needed) |
| `cargo fmt --all --check` | PASS (exit 0) |
| `cargo build --workspace` | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | PASS (no warnings) |
| `cargo test --workspace` | PASS |
| `cargo llvm-cov nextest -p hermit-health --fail-under-lines 80 --summary-only` | PASS |

### Test detail (`hermit-health`)

- 8 unit tests — all passed.
- 3 doctests — all passed (`FnChecker::new`, `health_router`, crate-level example).

Unit tests cover: `FnChecker` name/ok/err pass-through, liveness always-OK,
readiness with no checkers, all-OK (200), one-failing (503 + failing name), and the
OpenAPI spec exposing the probe paths.

## Coverage

`cargo llvm-cov nextest --fail-under-lines 80` (gate floor 80).

| File | Line cover | Missed lines |
| --- | --- | --- |
| `checker.rs` | 94.59% | 2 |
| `lib.rs` | 71.43% | 2 |
| `router.rs` | 100.00% | 0 |
| **TOTAL** | **97.08%** | **4** |

- Reported line coverage: **97.08%** — gate passed (rounds: 1).
- Region cover 96.09%, function cover 90.48%.

### Honest note on the uncovered lines

The 4 uncovered lines are the lowest-value paths, not gaps in probe logic:

- `lib.rs` (71.43%) drags the total down. The uncovered functions there are
  incidental (e.g. derived/plumbing paths not exercised by a direct test), while the
  security-relevant behavior — router wiring and OpenAPI path exposure — is covered via
  `openapi_spec_has_probe_paths` and the router tests.
- `router.rs`, which contains the actual liveness/readiness decision logic (200 vs 503,
  failing-checker name propagation), is at **100%** line and function coverage.
- `checker.rs` misses 2 lines (94.59%); the `Checker`/`FnChecker` contract paths that
  matter (name, ok, err) are all exercised.

No tests were skipped, ignored, or silently green. No `#[ignore]`, no gated-behind-env
tests in this crate.

## Summary

`hermit-health` passes the full INTEGRATE gate: formatting, workspace build, clippy
(`-D warnings`), workspace tests, and the 80% line-coverage floor (actual 97.08%). The
core probe/router logic is fully covered; the small residual uncovered lines are in
non-decision plumbing.
