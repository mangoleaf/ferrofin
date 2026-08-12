# Contributing to Ferrofin

Thanks for your interest in Ferrofin — a from-scratch Rust implementation of the
Jellyfin server API. This guide covers how to build, test, and submit changes.

Ferrofin is **GPL-3.0-only** (it is a derivative of Jellyfin's GPL-3.0 server crates).
By contributing you agree your work is licensed under the same terms.

## Development setup

Rust workspace, edition 2024, toolchain pinned to **1.97.1** (stable — see
`rust-toolchain.toml`, which rustup honors automatically).

```bash
cargo build --workspace
cargo run -p ferrofin-server -- --data-dir ./data --bind 127.0.0.1 --port 8096
```

On a fresh database the server seeds an admin user and logs a generated password —
record it. ffmpeg/ffprobe are auto-discovered on `$PATH`; without them only
transcoding is disabled.

## Quality gates (CI enforces all of these)

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --workspace          # + cargo test --workspace --doc for doctests
```

Every `pub` item needs a `///` doc and pedantic clippy must pass (warnings are
errors in CI). New code needs tests: Ferrofin ports Jellyfin behavior faithfully,
so where upstream ships xUnit `[Theory]/[InlineData]` cases, transliterate them
into `rstest` `#[case]` tests — the C# expected values are the oracle.

**Coverage gate — ≥80% line coverage, per crate:**

```bash
cargo llvm-cov nextest -p <crate> --fail-under-lines 80 --summary-only
```

Gate each crate on its own; don't pass multiple `-p` flags to one run (that
checks the merged total and lets a weak crate hide behind a strong one).

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

1. Branch off `main` (trunk-based development; `main` stays releasable).
2. Keep the change scoped; run the quality gates locally before pushing.
3. Open a merge request against `main`. CI runs fmt/clippy/tests + the coverage gate.
4. If you add or rename an HTTP route, keep the contract-superset test green
   (`crates/ferrofin-api/tests/contract_superset.rs`) — the registered route table
   must remain a superset of the vendored Jellyfin OpenAPI spec.

## Reporting bugs and security issues

Functional bugs: open an issue. Security vulnerabilities: **do not** open a public
issue — follow [SECURITY.md](SECURITY.md).
