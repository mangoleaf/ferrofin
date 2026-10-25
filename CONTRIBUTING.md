# Contributing to Ferrofin

Thanks for your interest in Ferrofin — a from-scratch Rust implementation of the
Jellyfin server API. This guide covers how to build, test, and submit changes.

Ferrofin is **GPL-3.0-only** (it is a derivative of Jellyfin's GPL-3.0 server crates).
By contributing you agree your work is licensed under the same terms.

## Development setup

Rust workspace, edition 2024, toolchain pinned to **1.98.1** (stable — see
`rust-toolchain.toml`, which rustup honors automatically).

```bash
cargo build --workspace
cargo run -p ferrofin-server -- --data-dir ./data --bind 127.0.0.1 --port 8096
```

On a fresh database the server seeds an admin user with no password (Jellyfin's default),
which the setup wizard at `/web` sets; `FERROFIN_ADMIN_PASSWORD` seeds one instead.
ffmpeg/ffprobe are auto-discovered on `$PATH`; without them only transcoding is disabled.

## Quality gates (CI enforces all of these)

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --workspace          # + cargo test --workspace --doc for doctests
cargo deny --locked check              # advisories, licenses, bans, sources (deny.toml)
cargo machete apps crates              # declared but unused dependencies
scripts/check-workspace-deps.py        # every dependency through [workspace.dependencies]
```

Every `pub` item needs a `///` doc and pedantic clippy must pass (warnings are
errors in CI). New code needs tests: Ferrofin ports Jellyfin behavior faithfully,
so where upstream ships xUnit `[Theory]/[InlineData]` cases, transliterate them
into `rstest` `#[case]` tests — the C# expected values are the oracle.

**The parity record is part of done.** A change to a handler or a core path either
adds or updates the parity row it touches, after comparing that code against the
upstream Jellyfin C# at `UPSTREAM_TAG`, or says in the PR why the behaviour it touches
is unchanged. The row is the operation's `handlers::VERIFIED` row, or the
`handlers::VERIFIED_SUBSYSTEMS` row of a shared subsystem the change sits in (query
translation, DTO fields, user data, access filtering, images, paging and sort), which
operation rows reference through `refs`. A re-checked row leaves
`VERIFIED_AGAINST_EARLIER_PIN` (or `VERIFIED_SUBSYSTEMS_AGAINST_EARLIER_PIN`). Live
parity runs and benchmark shape checks are supporting evidence; they never add a row.

**Dependencies.** Declare a dependency once, in the root `[workspace.dependencies]`, and
reference it as `dep.workspace = true`; `scripts/check-workspace-deps.py` refuses an inline
version, and cargo-deny a workspace entry nothing uses. When `cargo deny` reports an
advisory, even one published after your change, upgrade the crate if a fixed version exists
(`cargo update -p <crate> --precise <version>`, or a manifest bump for a new major). An
advisory with no fix, or one that does not reach Ferrofin's use of the crate, goes into
`deny.toml`'s `[advisories] ignore` with its reason, and leaves it once the crate is fixed.
A new license is a compatibility decision against GPL-3.0-only, not a fix.

**Coverage gate — ≥80% line coverage, per crate:**

```bash
crate=ferrofin-core   # the crate to gate
others=$(basename -a crates/*/ | grep -v "^$crate\$" | paste -sd'|')
cargo llvm-cov nextest -p "$crate" --fail-under-lines 80 --summary-only \
  --ignore-filename-regex "(crates/($others)|apps)/"
```

This is CI's form: both pipelines run it per crate from `scripts/coverage-gate.sh`,
split into three parallel shards. `-p` alone still instruments the
crate's workspace dependencies, so a crate whose tests link another crate (e.g.
`ferrofin-wasm`'s scan tests link `ferrofin-core`) would count that crate's uncovered
source in its total; the regex leaves only the crate's own files.

Gate each crate on its own; don't pass multiple `-p` flags to one run (that
checks the merged total and lets a weak crate hide behind a strong one).

**Performance is measured per release, not per PR.** Body-diff correctness is not a
latency signal: a large slowdown can land "green." So every release is benchmarked
before it is tagged, and a regression blocks that release (see [`RELEASE.md`](RELEASE.md)).
A PR does not need a before/after. If it touches `ferrofin-core`, `ferrofin-db`,
`ferrofin-api`, or the query/repository/DTO paths, say so in the description so the
release run can be read against it.

The benchmark suite lives in [`gates/bench/`](gates/bench/README.md): rebuild `ferrofin:bench`, run
`gates/bench/run.sh`, and compare with `python3 gates/bench/report.py --serve`.
`gates/bench/run.sh --servers ferrofin --only loaded` on two builds is the cheap form when
chasing a regression. It is deliberately **not** a CI gate. Budget for the setup if you
run it: it needs `docker`, `k6`, `jq`, `taskset`, `curl` and `python3` (plus `ffmpeg`
with libx264/libx265, `ffprobe` and Python `Pillow` for the test data), the two Jellyfin
images must be pulled, the test data is generated locally and gitignored
(`gates/bench/testdata/build.sh`, ~20 min and ~17 GB), and a full run needs a quiet machine for
about forty minutes. `gates/bench/README.md` has the complete recipe.

> Green tests are necessary, not sufficient. When you touch a data, auth, or
> stateful path, **run the binary and exercise it over real HTTP** — several
> bugs here passed their tests and were only caught by hitting the server.

## Commits: Conventional Commits + sign-off

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org)
— this drives the automated changelog (`cliff.toml` → `CHANGELOG.md`):

```
feat(sessions): remote-control bus-registered sockets
fix(router): merge repeated query params so array params don't 400
```

Types: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `ci`, `build`, `chore`.
A breaking change adds `!` (`feat!:`) or a `BREAKING CHANGE:` footer.

Sign off every commit (Developer Certificate of Origin):

```bash
git commit -s
```

## Submitting changes

1. Pick the branch by what the change is:
   - **A bug fix for the version already released** (the latest `vX.Y.Z` tag): branch off
     and target `main`. `main` carries the released line and can ship patch releases
     (`vX.Y.Z+1`); its fixes are merged into the open release branch.
   - **A feature, or a fix to a feature that is not released yet:** branch off and target
     the current `release/vX.Y.Z` branch. Only one exists at a time; it becomes the next
     feature release.
2. Keep the change scoped; run the quality gates locally before pushing.
3. Open a pull request against that branch. CI runs fmt/clippy/tests + the coverage gate.
4. If you add or rename an HTTP route, keep the contract-superset test green
   (`crates/ferrofin-api/tests/contract_superset.rs`) — the registered route table
   must remain a superset of the vendored Jellyfin OpenAPI spec.

## Reporting bugs and security issues

Functional bugs: open an issue. Security vulnerabilities: **do not** open a public
issue — follow [SECURITY.md](SECURITY.md).
