#!/usr/bin/env bats
# Tests for scripts/version.sh. Each test builds a throwaway git repo so the
# script's output is asserted against a known tag/commit history.

setup() {
  SCRIPT="$BATS_TEST_DIRNAME/../version.sh"
  REPO="$(mktemp -d)"
  cd "$REPO"
  git init -q
  git config user.email t@t
  git config user.name t
  git config commit.gpgsign false
  git commit -q --allow-empty -m "chore: init"
  # Neutralize CI/override env so tests are deterministic regardless of caller.
  unset CI_COMMIT_TAG FORCE_VERSION
}

teardown() {
  rm -rf "$REPO"
}

# Commit touching a build-relevant path (bumps the `image` count).
build_commit() {
  mkdir -p crates
  echo "$RANDOM" >> crates/x.rs
  git add crates/x.rs
  git commit -q -m "$1"
}

# --- next: conventional-commit bump ---------------------------------------

@test "next: feat since tag -> minor" {
  git tag v0.4.1
  git commit -q --allow-empty -m "feat: a feature"
  run "$SCRIPT" next
  [ "$status" -eq 0 ]
  [ "$output" = "v0.5.0" ]
}

@test "next: only fix since tag -> patch" {
  git tag v0.4.1
  git commit -q --allow-empty -m "fix: a fix"
  git commit -q --allow-empty -m "docs: notes"
  run "$SCRIPT" next
  [ "$output" = "v0.4.2" ]
}

@test "next: breaking (type!:) on 0.x -> minor, never 1.0.0" {
  git tag v0.4.1
  git commit -q --allow-empty -m "refactor!: big change"
  run "$SCRIPT" next
  [ "$output" = "v0.5.0" ]
}

@test "next: breaking on 1.x -> major" {
  git tag v1.2.3
  git commit -q --allow-empty -m "feat!: breaking feature"
  run "$SCRIPT" next
  [ "$output" = "v2.0.0" ]
}

@test "next: body mentioning BREAKING CHANGE does NOT bump major (subject only)" {
  git tag v0.4.1
  git commit -q --allow-empty -m "fix: a fix" -m "docs: this is not a BREAKING CHANGE"
  run "$SCRIPT" next
  [ "$output" = "v0.4.2" ]
}

@test "next: highest tag chosen, not most-recent" {
  git tag v0.4.1
  git commit -q --allow-empty -m "feat: x"; git tag v0.5.0
  git tag v0.4.2   # lower version, created later
  git commit -q --allow-empty -m "fix: y"
  run "$SCRIPT" next
  [ "$output" = "v0.5.1" ]
}

@test "next: FORCE_VERSION overrides the computed bump" {
  git tag v0.4.1
  git commit -q --allow-empty -m "fix: a fix"
  FORCE_VERSION=v9.9.9 run "$SCRIPT" next
  [ "$output" = "v9.9.9" ]
}

@test "next: no tags yet -> patch from 0.0.0" {
  run "$SCRIPT" next
  [ "$output" = "v0.0.1" ]
}

# --- image: release vs dev version ----------------------------------------

@test "image: on a release tag -> the tag without leading v" {
  git tag v0.5.0
  CI_COMMIT_TAG=v0.5.0 run "$SCRIPT" image
  [ "$output" = "0.5.0" ]
}

@test "image: on an -rc tag -> tag without leading v" {
  CI_COMMIT_TAG=v0.5.0-rc.1 run "$SCRIPT" image
  [ "$output" = "0.5.0-rc.1" ]
}

@test "image: dev version = base-<build-relevant count>-<sha12>" {
  git tag v0.4.1
  build_commit "feat: code change"          # build-relevant
  git commit -q --allow-empty -m "docs: not build-relevant"
  build_commit "fix: another code change"   # build-relevant
  run "$SCRIPT" image
  [ "$status" -eq 0 ]
  sha=$(git rev-parse --short=12 HEAD)
  [ "$output" = "0.4.1-2-${sha}" ]          # count is 2 (docs commit excluded)
}

@test "image: dev sha component is 12 hex chars" {
  git tag v0.4.1
  build_commit "feat: x"
  run "$SCRIPT" image
  sha_part="${output##*-}"
  [ "${#sha_part}" -eq 12 ]
}

# --- usage ----------------------------------------------------------------

@test "unknown subcommand exits 2 with usage" {
  run "$SCRIPT" bogus
  [ "$status" -eq 2 ]
  [[ "$output" == *"usage:"* ]]
}
