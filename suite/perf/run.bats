#!/usr/bin/env bats
# Tests for the pure helpers in run.sh — the footprint-metric logic that shipped three
# bugs (awk GiB-strip, page-cache in the sample, stale appended file). These lock the fixes.
#   bats run.bats      (needs `bats`; on Arch: sudo pacman -S bats)

# Source only the helpers (BENCH_TEST_SOURCE makes run.sh return before doing any work).
# Subshell isolates run.sh's `set -euo pipefail` from the bats runner.
helpers() { bash -c 'BENCH_TEST_SOURCE=1 source ./run.sh; '"$*"; }

setup() { cd "$BATS_TEST_DIRNAME"; }

@test "sourcing with BENCH_TEST_SOURCE stops before running a benchmark" {
  run helpers 'echo SOURCED_OK'
  [ "$status" -eq 0 ]
  [ "$output" = "SOURCED_OK" ]
}

# --- anon_mib: cgroup-v2 memory.stat -> anonymous MiB (page cache excluded) ---

@test "anon_mib converts the anon byte count to MiB" {
  run bash -c 'printf "anon 184549376\n" | { BENCH_TEST_SOURCE=1 source ./run.sh; anon_mib; }'
  [ "$output" = "176.0" ]
}

@test "anon_mib reads only 'anon', not file/cache or anon_thp" {
  run bash -c 'printf "file 4294967296\nanon 10485760\nanon_thp 999999999\n" | { BENCH_TEST_SOURCE=1 source ./run.sh; anon_mib; }'
  # Only the 10 MiB anon line counts; the 4 GiB file-cache line is the bug we removed.
  [ "$output" = "10.0" ]
}

@test "anon_mib emits nothing when there is no anon line" {
  run bash -c 'printf "file 123\n" | { BENCH_TEST_SOURCE=1 source ./run.sh; anon_mib; }'
  [ -z "$output" ]
}

# --- peak: max of a file of plain numbers ---

@test "peak returns the largest sample, rounded to an integer" {
  printf '7.4\n1256\n184\n1256.9\n' > "$BATS_TEST_TMPDIR/p"
  run helpers "peak '$BATS_TEST_TMPDIR/p'"
  [ "$output" = "1257" ]
}

@test "peak of a single sample is that sample" {
  printf '176.0\n' > "$BATS_TEST_TMPDIR/p"
  run helpers "peak '$BATS_TEST_TMPDIR/p'"
  [ "$output" = "176" ]
}

@test "peak of an empty file is 0" {
  : > "$BATS_TEST_TMPDIR/p"
  run helpers "peak '$BATS_TEST_TMPDIR/p'"
  [ "$output" = "0" ]
}

@test "peak of a missing file is ?" {
  run helpers "peak '$BATS_TEST_TMPDIR/does-not-exist'"
  [ "$output" = "?" ]
}

@test "peak treats input as plain numbers (a stray 4GiB is 4, not 4096)" {
  # Regression guard: the old awk mis-rebuilt $0 and read GiB suffixes wrong. The sampler
  # now emits bare MiB numbers, so peak must NOT reinterpret suffixes — 4GiB parses as 4.
  printf '30\n4GiB\n25\n' > "$BATS_TEST_TMPDIR/p"
  run helpers "peak '$BATS_TEST_TMPDIR/p'"
  [ "$output" = "30" ]
}

# --- end-to-end: sampled memory.stat -> anon_mib -> file -> peak ---

@test "anon_mib piped into peak yields the peak anon MiB" {
  bash -c 'printf "anon 52428800\nanon 184549376\nanon 104857600\n" | { BENCH_TEST_SOURCE=1 source ./run.sh; anon_mib; }' > "$BATS_TEST_TMPDIR/rss"
  run helpers "peak '$BATS_TEST_TMPDIR/rss'"
  [ "$output" = "176" ]
}
