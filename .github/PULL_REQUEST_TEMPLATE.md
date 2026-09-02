<!-- Thanks for contributing! See CONTRIBUTING.md. Keep the change scoped. -->

**What & why**
<!-- What this changes and the motivation. Link any issue. -->

**Checklist**
- [ ] Commit messages follow Conventional Commits and are signed off (`git commit -s`)
- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo nextest run --workspace` + doctests pass
- [ ] Per-crate coverage still ≥80% for touched crates
- [ ] If a route was added/renamed: `contract_superset` test still green
- [ ] If a perf-sensitive path was touched: measured before/after included in the description
- [ ] Ran the server and exercised the change over real HTTP (for data/auth/stateful paths)
