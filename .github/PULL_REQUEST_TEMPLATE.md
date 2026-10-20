<!-- Thanks for contributing! See CONTRIBUTING.md. Keep the change scoped. -->

## What
<!-- The change, user-visible behaviour first. -->

## Why
<!-- The problem and its impact. Link any issue; cite the upstream Jellyfin code for parity fixes. -->

## How
<!-- The approach, the key files, and anything a reviewer should look at closely. -->

## Verification
<!-- The commands you ran and their results, the live HTTP check, the tests you added. -->

## Checklist
- [ ] Commit subjects follow Conventional Commits (`type(scope): subject`) and are signed off (`git commit -s`)
- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo nextest run --workspace` + doctests pass
- [ ] Per-crate coverage still ≥80% for touched crates (CI's command, see CONTRIBUTING.md)
- [ ] If a route was added/renamed: `contract_superset` test still green
- [ ] If a handler or core path changed: compared against the upstream C# at `UPSTREAM_TAG`, and the operation's `handlers::VERIFIED` row or the subsystem's `handlers::VERIFIED_SUBSYSTEMS` row added or updated with its divergences (a re-checked row also leaves its earlier-pin list), or the description says why the touched behaviour is unchanged
- [ ] If the migration chain, the adoption gate or the boot repairs changed: adoption validation run (`adoption/README.md`)
- [ ] If a perf-sensitive path (`ferrofin-core`, `ferrofin-db`, `ferrofin-api`) was touched: noted in the description (performance is benchmarked per release, not per PR)
- [ ] Ran the server and exercised the change over real HTTP (for data/auth/stateful paths)
