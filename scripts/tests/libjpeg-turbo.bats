#!/usr/bin/env bats
# Tests for scripts/libjpeg-turbo.sh. Each test runs the script in a throwaway tree with a fake
# `curl`, so nothing is downloaded or compiled: what is asserted is what the script refuses and
# what it leaves alone.

setup() {
  TREE="$BATS_TEST_TMPDIR/tree"
  mkdir -p "$TREE/scripts" "$BATS_TEST_TMPDIR/bin"
  cp "$BATS_TEST_DIRNAME/../libjpeg-turbo.sh" "$TREE/scripts/"
  VERSION=$(sed -n 's/^VERSION=//p' "$TREE/scripts/libjpeg-turbo.sh")
  SHA256=$(sed -n 's/^SHA256=//p' "$TREE/scripts/libjpeg-turbo.sh")
  PREFIX="$TREE/scripts/libjpeg-turbo.sh.out/$VERSION"
  export CALLS="$BATS_TEST_TMPDIR/calls"
  # Writes something that is not the release to the -o file, as a tampered mirror would.
  cat >"$BATS_TEST_TMPDIR/bin/curl" <<'FAKE'
#!/bin/sh
echo "$*" >>"$CALLS"
while [ $# -gt 0 ]; do
  [ "$1" = -o ] && out=$2
  shift
done
echo "not the release" >"$out"
FAKE
  chmod +x "$BATS_TEST_TMPDIR/bin/curl"
  PATH="$BATS_TEST_TMPDIR/bin:$PATH"
}

@test "a download with another checksum is refused and nothing is installed" {
  run "$TREE/scripts/libjpeg-turbo.sh"
  [ "$status" -eq 1 ]
  [[ "$output" == *"does not have the pinned SHA-256"* ]]
  [[ "$(cat "$CALLS")" == *"/releases/download/$VERSION/libjpeg-turbo-$VERSION.tar.gz" ]]
  # Not even the directory cargo is pointed at: a build then stops with its path.
  [ ! -e "$PREFIX" ]
}

@test "a build already there is left alone" {
  mkdir -p "$PREFIX"
  echo "$VERSION $SHA256 native" >"$PREFIX/stamp"
  run "$TREE/scripts/libjpeg-turbo.sh"
  [ "$status" -eq 0 ]
  [ ! -e "$CALLS" ]
}

@test "a build for another compiler is not taken for this one" {
  mkdir -p "$PREFIX"
  echo "$VERSION $SHA256 aarch64-linux-gnu.2.34" >"$PREFIX/stamp"
  run "$TREE/scripts/libjpeg-turbo.sh"
  [ "$status" -eq 1 ]
  [ -e "$CALLS" ]
  [ ! -e "$PREFIX" ]
}

@test "--check tells a binary linking the pinned version from any other" {
  printf 'ELF\0libjpeg-turbo version %s (build 20261026)\0' "$VERSION" >"$BATS_TEST_TMPDIR/pinned"
  printf 'ELF\0libjpeg-turbo version 3.1.0 (build 20241212)\0' >"$BATS_TEST_TMPDIR/older"
  run "$TREE/scripts/libjpeg-turbo.sh" --check "$BATS_TEST_TMPDIR/pinned"
  [ "$status" -eq 0 ]
  run "$TREE/scripts/libjpeg-turbo.sh" --check "$BATS_TEST_TMPDIR/older"
  [ "$status" -eq 1 ]
  [[ "$output" == *"does not link libjpeg-turbo $VERSION"* ]]
  [ ! -e "$CALLS" ]
}

@test "cargo looks for the library where the pinned version is installed" {
  # .cargo/config.toml names the directory itself: it has to move with the script's VERSION.
  grep -qF "\"scripts/libjpeg-turbo.sh.out/$VERSION/lib\"" "$BATS_TEST_DIRNAME/../../.cargo/config.toml"
}

@test "an unknown argument is refused" {
  run "$TREE/scripts/libjpeg-turbo.sh" --prefix /somewhere
  [ "$status" -eq 1 ]
  [[ "$output" == *usage* ]]
}
