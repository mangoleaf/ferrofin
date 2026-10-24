#!/usr/bin/env bats
# Tests for scripts/coverage-gate.sh. Each test copies the script into a throwaway tree with
# stub crate directories and a fake `cargo` that records its arguments, so shard membership,
# the guards and the per-crate exclusion regex are asserted without compiling anything.

setup() {
  TREE="$BATS_TEST_TMPDIR/tree"
  mkdir -p "$TREE/scripts" "$BATS_TEST_TMPDIR/bin"
  cp "$BATS_TEST_DIRNAME/../coverage-gate.sh" "$TREE/scripts/"
  for c in core api wasm livetv extensions traits util db; do
    mkdir -p "$TREE/crates/ferrofin-$c"
  done
  # $4 is the crate: `cargo llvm-cov nextest -p <crate> ...`
  # shellcheck disable=SC2016  # expands when the stub runs, not here
  printf '#!/bin/sh\necho "$*" >>"%s/calls"\n[ "$FAIL_CRATE" != "$4" ]\n' \
    "$BATS_TEST_TMPDIR" >"$BATS_TEST_TMPDIR/bin/cargo"
  chmod +x "$BATS_TEST_TMPDIR/bin/cargo"
  PATH="$BATS_TEST_TMPDIR/bin:$PATH"
  SCRIPT="$TREE/scripts/coverage-gate.sh"
}

# The crates cargo was asked to gate, sorted.
gated() { awk '{print $4}' "$BATS_TEST_TMPDIR/calls" | sort | paste -sd' '; }

@test "core shard gates ferrofin-core only" {
  run "$SCRIPT" core
  [ "$status" -eq 0 ]
  [ "$(gated)" = "ferrofin-core" ]
}

@test "heavy shard gates the four named crates" {
  run "$SCRIPT" heavy
  [ "$status" -eq 0 ]
  [ "$(gated)" = "ferrofin-api ferrofin-extensions ferrofin-livetv ferrofin-wasm" ]
}

@test "rest shard is every other crate, never ferrofin-traits" {
  run "$SCRIPT" rest
  [ "$status" -eq 0 ]
  [ "$(gated)" = "ferrofin-db ferrofin-util" ]
}

@test "a new crate directory lands in rest" {
  mkdir "$TREE/crates/ferrofin-newcomer"
  run "$SCRIPT" rest
  [ "$status" -eq 0 ]
  [ "$(gated)" = "ferrofin-db ferrofin-newcomer ferrofin-util" ]
}

@test "the three shards together cover every crate except ferrofin-traits" {
  for s in core heavy rest; do "$SCRIPT" "$s"; done
  [ "$(gated)" = "ferrofin-api ferrofin-core ferrofin-db ferrofin-extensions ferrofin-livetv ferrofin-util ferrofin-wasm" ]
}

@test "a crate named in heavy with no directory fails the script" {
  rmdir "$TREE/crates/ferrofin-livetv"
  run "$SCRIPT" rest
  [ "$status" -eq 2 ]
  [[ "$output" == *"crates/ferrofin-livetv"* ]]
  [ ! -e "$BATS_TEST_TMPDIR/calls" ]
}

@test "each crate excludes every sibling, including ferrofin-traits, and apps" {
  run "$SCRIPT" core
  grep -q -- '--ignore-filename-regex (crates/(ferrofin-api|ferrofin-db|ferrofin-extensions|ferrofin-livetv|ferrofin-traits|ferrofin-util|ferrofin-wasm)|apps)/' \
    "$BATS_TEST_TMPDIR/calls"
}

@test "a failing crate fails the shard but the others still run" {
  FAIL_CRATE=ferrofin-api run "$SCRIPT" heavy
  [ "$status" -eq 1 ]
  [[ "$output" == *"Tests or coverage failed for ferrofin-api"* ]]
  [ "$(gated)" = "ferrofin-api ferrofin-extensions ferrofin-livetv ferrofin-wasm" ]
}

@test "an unknown shard is a usage error" {
  run "$SCRIPT" bogus
  [ "$status" -eq 2 ]
}
