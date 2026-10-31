#!/usr/bin/env bats
# Tests for cliff.toml. The release notes placeholder (release.yml) and the .deb changelog
# come from `git-cliff --current`; it must see only final vX.Y.Z tags and skip noise commits.
# Each test builds a throwaway git repo so the output is asserted against a known history.

setup() {
  CONFIG="$BATS_TEST_DIRNAME/../../cliff.toml"
  REPO="$BATS_TEST_TMPDIR/repo"
  mkdir -p "$REPO"
  cd "$REPO" || exit 1
  git init -q
  git config user.email t@t
  git config user.name t
  git config commit.gpgsign false
}

@test "current: one section for the final tag, every real commit, no noise" {
  git commit -q --allow-empty -m "feat: first release"
  git tag v1.0.0
  git commit -q --allow-empty -m "feat: before the chart tag"
  git commit -q --allow-empty -m "Merge pull request #7 from x/y"
  git commit -q --allow-empty -m "wip: scratch"
  git commit -q --allow-empty -m "Wip more scratch"
  git tag chart-v1.0.1
  git commit -q --allow-empty -m "fix: before the rc"
  git commit -q --allow-empty -m "style: reformat"
  git tag v1.1.0-rc.1
  git commit -q --allow-empty -m "perf: after the rc"
  git commit -q --allow-empty -m "docs: before the tag"
  git tag v1.1.0

  run git-cliff --config "$CONFIG" --current --strip header
  [ "$status" -eq 0 ]
  [ "$(grep -c '^## \[' <<< "$output")" -eq 1 ]
  grep -q '^## \[1\.1\.0\]' <<< "$output"
  for msg in "before the chart tag" "before the rc" "after the rc" "before the tag"; do
    grep -qi "$msg" <<< "$output"   # git-cliff capitalises the first letter
  done
  # not the previous final tag's own commit, and none of the noise (bats ignores a bare `!`)
  if grep -qiE 'first release|merge|wip|reformat' <<< "$output"; then return 1; fi
}
