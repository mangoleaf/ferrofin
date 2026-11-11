# Vendored icu_collator 2.4.0

This is the crates.io `icu_collator` 2.4.0 package with one change. Only its
`src/`, `Cargo.toml` and `LICENSE` are vendored; its README, `tests/`,
`benches/` and packaging metadata are not. The root `Cargo.toml` patches it
in through `[patch.crates-io]`.

## The change

`src/elements.rs` uses the shared root Jamo shortcut only for an untailored
Jamo. A tailoring's own CE32 (Korean search collation) keeps its expansions
and prefixes. Korean `search` collation needs it: genrb drops 67 modern Jamo
entries from that tailoring, and without the fix the root shortcut overrides
the ones restored in the generated data.

2.4.0 still has the unconditional shortcut (its `src/elements.rs` around line
1504, `if let Some(&jamo) = self.jamo.get(jamo_index)`), so the change is
re-applied unchanged.

## Upstream tests

The upstream suite includes Unicode's CLDR collation conformance files
(about 5 MB). It is not vendored. To run it against this source, copy the
`tests/` directory and the `[[test]]` and `[dev-dependencies]` sections from
the crates.io package back into a scratch copy, then run:

```sh
cargo test --test tests
```

Last run on 2026-11-07 with the patch applied: 64 passed, 0 failed.
