#!/usr/bin/env bats
# Tests for scripts/ci-config-check.sh. Each test builds a throwaway git repo with a
# base commit and a branch, so the diff classification is asserted on known histories.

setup() {
  SCRIPT="$BATS_TEST_DIRNAME/../ci-config-check.sh"
  REPO="$BATS_TEST_TMPDIR/repo"
  mkdir -p "$REPO/.github/workflows" "$REPO/crates" "$BATS_TEST_TMPDIR/bin"
  cd "$REPO" || exit 1
  git init -q -b main
  git config user.email t@t
  git config user.name t
  git config commit.gpgsign false
  printf 'on: push\njobs: {}\n' >.github/workflows/ci.yml
  echo 'stages: [test]' >.gitlab-ci.yml
  echo 'fn main() {}' >crates/a.rs
  git add -A
  git commit -q -m "chore: base"
  git checkout -q -b topic
  # Stubs for the optional linters, so the result does not depend on what the host has installed.
  for tool in actionlint zizmor; do
    # shellcheck disable=SC2016  # $* and $STUB_FAIL expand when the stub runs, not here
    printf '#!/bin/sh\necho "%s $*" >>"%s/calls"\n[ -z "$STUB_FAIL" ] || [ "$STUB_FAIL" != "%s" ]\n' \
      "$tool" "$BATS_TEST_TMPDIR" "$tool" >"$BATS_TEST_TMPDIR/bin/$tool"
    chmod +x "$BATS_TEST_TMPDIR/bin/$tool"
  done
  PATH="$BATS_TEST_TMPDIR/bin:$PATH"
}

commit_file() {
  mkdir -p "$(dirname "$1")"
  echo "$RANDOM" >>"$1"
  git add "$1"
  git commit -q -m "change $1"
}

# --- only: is the diff CI configuration alone? ------------------------------

@test "only: a workflow change is CI-config-only" {
  commit_file .github/workflows/ci.yml
  run "$SCRIPT" only main
  [ "$status" -eq 0 ]
}

@test "only: .gitlab-ci.yml plus Markdown is CI-config-only" {
  commit_file .gitlab-ci.yml
  commit_file docs/NOTES.md
  run "$SCRIPT" only main
  [ "$status" -eq 0 ]
}

@test "only: Markdown alone is not (it is the docs-only exception)" {
  commit_file README.md
  run "$SCRIPT" only main
  [ "$status" -eq 1 ]
}

@test "only: any source file alongside the YAML takes the full path" {
  commit_file .gitlab-ci.yml
  commit_file crates/a.rs
  run "$SCRIPT" only main
  [ "$status" -eq 1 ]
}

@test "only: scripts/ and Cargo.toml are not CI config" {
  commit_file .github/workflows/ci.yml
  commit_file scripts/x.sh
  run "$SCRIPT" only main
  [ "$status" -eq 1 ]
  git reset -q --hard HEAD~1
  commit_file Cargo.toml
  run "$SCRIPT" only main
  [ "$status" -eq 1 ]
}

@test "only: a source file renamed into Markdown is not CI-config-only" {
  commit_file .gitlab-ci.yml
  mv crates/a.rs docs.md
  git add -A
  git commit -q -m "move source into markdown"
  run "$SCRIPT" only main
  [ "$status" -eq 1 ]
}

@test "only: a deleted source file takes the full path" {
  commit_file .gitlab-ci.yml
  rm crates/a.rs
  git add -A
  git commit -q -m "delete source"
  run "$SCRIPT" only main
  [ "$status" -eq 1 ]
}

@test "only: Markdown under apps/, gates/adoption/, gates/verify/ and scripts/ takes the full path" {
  for f in apps/s/README.md gates/adoption/README.md gates/verify/README.md scripts/README.md; do
    git reset -q --hard main
    commit_file .gitlab-ci.yml
    commit_file "$f"
    run "$SCRIPT" only main
    [ "$status" -eq 1 ]
  done
}

@test "only: Markdown inside the source tree takes the full path" {
  commit_file .gitlab-ci.yml
  commit_file crates/README.md
  run "$SCRIPT" only main
  [ "$status" -eq 1 ]
}

@test "only: a Markdown path with odd characters is read unquoted" {
  commit_file .gitlab-ci.yml
  commit_file "docs/we ird\"name.md"
  run "$SCRIPT" only main
  [ "$status" -eq 0 ]
}

@test "only: an unknown base ref is an error" {
  commit_file .gitlab-ci.yml
  run "$SCRIPT" only no-such-ref
  [ "$status" -eq 2 ]
}

@test "only: other .github files are not workflows" {
  commit_file .github/workflows/ci.yml
  commit_file .github/dependabot.yml
  run "$SCRIPT" only main
  [ "$status" -eq 1 ]
}

@test "only: an empty diff is not CI-config-only" {
  run "$SCRIPT" only main
  [ "$status" -eq 1 ]
}

# --- check: validate the configuration -------------------------------------

@test "check: valid workflows pass and run the installed linters" {
  run "$SCRIPT" check
  [ "$status" -eq 0 ]
  grep -q '^actionlint .github/workflows/ci.yml' "$BATS_TEST_TMPDIR/calls"
  grep -q '^zizmor .github/workflows/ci.yml' "$BATS_TEST_TMPDIR/calls"
}

@test "check: a workflow that is not YAML fails and names the file" {
  printf 'jobs:\n  a: [unclosed\n' >.github/workflows/ci.yml
  run "$SCRIPT" check
  [ "$status" -eq 1 ]
  [[ "$output" == *".github/workflows/ci.yml"* ]]
}

@test "check: a broken .gitlab-ci.yml fails offline" {
  printf 'a: [unclosed\n' >.gitlab-ci.yml
  run "$SCRIPT" check
  [ "$status" -eq 1 ]
  [[ "$output" == *".gitlab-ci.yml"* ]]
}

@test "check: GitLab's !reference tag is not a syntax error" {
  printf 'a:\n  script:\n    - !reference [.b, script]\n' >.gitlab-ci.yml
  run "$SCRIPT" check
  [ "$status" -eq 0 ]
}

@test "check: an unknown argument is refused" {
  run "$SCRIPT" check --gitlb
  [ "$status" -eq 2 ]
}

@test "check: a YAML failure still runs the linters" {
  printf 'jobs:\n  a: [unclosed\n' >.github/workflows/ci.yml
  run "$SCRIPT" check
  [ "$status" -eq 1 ]
  [ -s "$BATS_TEST_TMPDIR/calls" ]
}

@test "check: a failing linter fails the check" {
  STUB_FAIL=zizmor run "$SCRIPT" check
  [ "$status" -eq 1 ]
}

@test "check: without --gitlab the GitLab lint is skipped with a note" {
  run "$SCRIPT" check
  [[ "$output" == *"SKIP GitLab CI Lint"* ]]
}

@test "check: --gitlab fails on valid:false and prints the errors" {
  printf '#!/bin/sh\necho %s\n' "'{\"valid\":false,\"errors\":[\"jobs:x interruptible should be a boolean value\"],\"warnings\":[]}'" >"$BATS_TEST_TMPDIR/bin/glab"
  chmod +x "$BATS_TEST_TMPDIR/bin/glab"
  run "$SCRIPT" check --gitlab
  [ "$status" -eq 1 ]
  [[ "$output" == *"should be a boolean value"* ]]
}

@test "check: --gitlab passes on valid:true" {
  printf '#!/bin/sh\necho %s\n' "'{\"valid\":true,\"errors\":[],\"warnings\":[]}'" >"$BATS_TEST_TMPDIR/bin/glab"
  chmod +x "$BATS_TEST_TMPDIR/bin/glab"
  run "$SCRIPT" check --gitlab
  [ "$status" -eq 0 ]
}

@test "check: --gitlab fails when the lint call itself fails" {
  printf '#!/bin/sh\nexit 1\n' >"$BATS_TEST_TMPDIR/bin/glab"
  chmod +x "$BATS_TEST_TMPDIR/bin/glab"
  run "$SCRIPT" check --gitlab
  [ "$status" -eq 1 ]
}
