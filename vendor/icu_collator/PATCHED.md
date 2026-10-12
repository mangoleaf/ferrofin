# Vendored icu_collator 2.3.1

This is the crates.io `icu_collator` 2.3.1 package with one change, and without
its `tests/` and `benches/`. The root `Cargo.toml` patches it in through
`[patch.crates-io]`.

## The change

`src/elements.rs` uses the shared root Jamo shortcut only for an untailored
Jamo. A tailoring's own CE32 (Korean search collation) keeps its expansions
and prefixes. See "Save image paths in NFO" in
`docs/reviews/dashboard-settings.md`.

## Upstream tests

The upstream suite includes Unicode's CLDR collation conformance files
(about 5 MB). It is not vendored. To run it against this source, copy the
`tests/` directory and the `[[test]]` and `[dev-dependencies]` sections from
the crates.io package back into a scratch copy, then run:

```sh
cargo test --test tests
```

Last run on 2026-10-11 with the patch applied: 63 passed, 0 failed.
