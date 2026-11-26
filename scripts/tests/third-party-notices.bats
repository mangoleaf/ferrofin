#!/usr/bin/env bats
# Tests for scripts/third-party-notices.sh. Each test runs the script in a throwaway tree with a fake
# `cargo` that records its arguments, writes the -o file as cargo-about does, and then warns or fails
# as the test asks, so the failure rules are asserted without cargo-about or a crate graph.

setup() {
  TREE="$BATS_TEST_TMPDIR/tree"
  mkdir -p "$TREE/scripts" "$BATS_TEST_TMPDIR/bin"
  cp "$BATS_TEST_DIRNAME/../third-party-notices.sh" "$TREE/scripts/"
  export CALLS="$BATS_TEST_TMPDIR/calls"
  cat >"$BATS_TEST_TMPDIR/bin/cargo" <<'EOF'
#!/bin/sh
echo "$*" >>"$CALLS"
[ "$1" = fetch ] && exit "${FETCH_EXIT:-0}"
while [ $# -gt 0 ]; do
  [ "$1" = -o ] && out=$2
  shift
done
echo notices >"$out"
[ -z "${ABOUT_STDERR:-}" ] || echo "$ABOUT_STDERR" >&2
exit "${ABOUT_EXIT:-0}"
EOF
  chmod +x "$BATS_TEST_TMPDIR/bin/cargo"
  PATH="$BATS_TEST_TMPDIR/bin:$PATH"
  cd "$TREE" || return 1
}

@test "a clean run fetches every crate, then writes the notices offline" {
  run scripts/third-party-notices.sh
  [ "$status" -eq 0 ]
  [ "$(sed -n 1p "$CALLS")" = "fetch --locked" ]
  [[ "$(sed -n 2p "$CALLS")" == "about generate --frozen --fail "* ]]
  [ "$(cat target/THIRD-PARTY-NOTICES.txt)" = notices ]
}

@test "a cargo-about warning with exit 0 fails and leaves no notices" {
  ABOUT_STDERR="WARN checksum mismatch" run scripts/third-party-notices.sh
  [ "$status" -eq 1 ]
  [[ "$output" == *"WARN checksum mismatch"* ]]
  [ ! -e target/THIRD-PARTY-NOTICES.txt ]
}

@test "a cargo-about failure fails and leaves no notices" {
  ABOUT_EXIT=1 run scripts/third-party-notices.sh
  [ "$status" -eq 1 ]
  [[ "$output" == *"exit 1"* ]]
  [ ! -e target/THIRD-PARTY-NOTICES.txt ]
}

@test "a failed fetch stops before cargo-about runs" {
  FETCH_EXIT=101 run scripts/third-party-notices.sh
  [ "$status" -ne 0 ]
  [ "$(wc -l <"$CALLS")" -eq 1 ]
}
