#!/usr/bin/env bash
# Checks for changes that touch only CI configuration, used by the local pre-push gate
# (the github-open-pr skill's ci_gates.sh) and run by the `shell-tests` job.
#
#   ci-config-check.sh only <base-ref>   exit 0 when the diff <base-ref>...HEAD touches CI configuration
#                                        (.github/workflows/*.yml|yaml, .gitlab-ci.yml) and nothing but
#                                        Markdown besides; exit 1 otherwise. Anything else, including
#                                        scripts/, crates/, apps/ and Cargo.*, is not CI-config-only.
#                                        Exit 2 when <base-ref> is not a ref (callers treat any non-zero
#                                        as "run the full gate"). Committed changes only.
#   ci-config-check.sh check [--gitlab]  validate the CI configuration in the working tree:
#                                        - every .github/workflows file and .gitlab-ci.yml parses as YAML;
#                                        - actionlint and zizmor run when installed (skipped with a note);
#                                        - every Rust toolchain pin names rust-toolchain.toml's channel, and
#                                          every tool pinned in several places names one version (PINS);
#                                        - --gitlab also runs GitLab's CI Lint on .gitlab-ci.yml. It needs
#                                          glab and a GitLab token, so it is a local-only step: CI never has
#                                          one, and the pipeline itself is the check there.
#                                        Runs every check; exit 1 if any failed.
set -uo pipefail

GITLAB_PROJECT=${GITLAB_PROJECT:-76} # GitLab project id for CI Lint (resolves includes and variables); override via the environment

# Every Rust toolchain pin and every tool version pinned in more than one place, one "<group> <file> <ERE>"
# per line; the last dotted number in each match is the version. Group rust must name rust-toolchain.toml's
# channel, any other group one version throughout. A line that matches nothing fails, so a file cannot lose
# all its pins unnoticed; one site of several reworded out of its pattern is not seen, so keep a pin's form,
# and give a new pin site its own line here. Only these files are read:
# dated prose such as docs/reviews/ records the toolchain a measurement was built with. Override via the
# environment (the tests use a small table).
PINS=${CI_CONFIG_PINS:-'
rust examples/wasm-hello/rust-toolchain.toml ^channel = "[0-9.]+"
rust mise.toml ^rust = \{ version = "[0-9.]+"
rust Dockerfile ^FROM rust:[0-9.]+
rust ci/Dockerfile rust[:-][0-9.]+
rust .gitlab-ci.yml ci:rust-[0-9.]+
rust .github/workflows/ci.yml toolchain: [0-9.]+
rust .github/workflows/ci.yml the [0-9.]+ branch
rust .github/workflows/release.yml toolchain: [0-9.]+
rust .github/workflows/release.yml the [0-9.]+ branch
rust CLAUDE.md pinned to \*\*[0-9.]+
rust CONTRIBUTING.md pinned to \*\*[0-9.]+
cargo-deny .github/workflows/ci.yml cargo-deny@[0-9.]+
cargo-deny .github/workflows/release.yml cargo-deny@[0-9.]+
cargo-deny ci/Dockerfile cargo-deny@[0-9.]+
cargo-machete .github/workflows/ci.yml cargo-machete@[0-9.]+
cargo-machete ci/Dockerfile cargo-machete@[0-9.]+
cargo-vet .github/workflows/ci.yml cargo-vet@[0-9.]+
cargo-vet ci/Dockerfile cargo-vet@[0-9.]+
cargo-audit .github/workflows/release.yml cargo-audit@[0-9.]+
cargo-auditable .github/workflows/release.yml cargo-auditable@[0-9.]+
cargo-auditable Dockerfile cargo-auditable@[0-9.]+
cargo-nfpm mise.toml ^"cargo:cargo-nfpm" = "[0-9.]+
cargo-nfpm .github/workflows/release.yml cargo-nfpm --version [0-9.]+
git-cliff mise.toml ^git-cliff = "[0-9.]+"
git-cliff .github/workflows/ci.yml git-cliff==[0-9.]+
git-cliff .github/workflows/release.yml git-cliff==[0-9.]+
git-cliff .gitlab-ci.yml git-cliff==[0-9.]+
sccache .gitlab-ci.yml SCCACHE_VERSION: "[0-9.]+
sccache .github/workflows/ci.yml version: v[0-9.]+
'}

# --no-renames lists a rename as delete + add, so moving a source file into a CI path still shows the
# source deletion; -z keeps odd path characters unquoted.
only() {
  local base=${1:?usage: ci-config-check.sh only <base-ref>} f ci=0 any=0
  git merge-base "$base" HEAD >/dev/null || return 2 # a bad ref is an error (2), not "touches source" (1)
  while IFS= read -r -d '' f; do
    any=1
    case $f in
      .github/workflows/*.yml | .github/workflows/*.yaml | .gitlab-ci.yml) ci=1 ;;
      crates/* | apps/* | scripts/* | gates/adoption/* | gates/verify/*) return 1 ;; # Markdown here is still source tree
      *.md) ;;
      *) return 1 ;;
    esac
  done < <(git diff --name-only --no-renames -z "$base"...HEAD --)
  [ "$any" = 1 ] && [ "$ci" = 1 ]
}

# GitLab CI uses custom tags (!reference); load them as plain values so only real syntax errors fail.
yaml_parses() {
  python3 - "$@" <<'PY'
import sys, yaml
class Loader(yaml.SafeLoader):
    pass
Loader.add_multi_constructor("!", lambda loader, suffix, node: None)
bad = 0
for path in sys.argv[1:]:
    try:
        with open(path) as f:
            yaml.load(f, Loader=Loader)
    except yaml.YAMLError as e:
        print(f"{path}: {e}")
        bad = 1
sys.exit(bad)
PY
}

pins() {
  local group file re found m v fail=0
  local -A want=() from=()
  want[rust]=$(sed -n 's/^channel = "\([^"]*\)".*/\1/p' rust-toolchain.toml 2>/dev/null)
  if [ -z "${want[rust]}" ]; then
    echo "rust-toolchain.toml: no channel"
    return 1
  fi
  from[rust]=rust-toolchain.toml
  while read -r group file re; do
    [ -n "$group" ] || continue
    if ! found=$(grep -n -o -E -- "$re" "$file" 2>/dev/null); then
      echo "$file: no $group pin matches /$re/"
      fail=1
      continue
    fi
    while IFS= read -r m; do # m is "<line>:<match>"
      v=$(grep -o -E '[0-9]+(\.[0-9]+)+' <<<"${m#*:}" | tail -n 1)
      if [ -z "${want[$group]:-}" ]; then
        want[$group]=$v
        from[$group]=$file:${m%%:*}
      elif [ "$v" != "${want[$group]}" ]; then
        echo "$file:${m%%:*}: $group $v, but ${from[$group]} names ${want[$group]}"
        fail=1
      fi
    done <<<"$found"
  done <<<"$PINS"
  [ "$fail" = 1 ] || echo "pins agree (rust ${want[rust]})"
  return $fail
}

gitlab_lint() {
  local out
  out=$(glab api --method POST "projects/$GITLAB_PROJECT/ci/lint" -F content=@.gitlab-ci.yml) || {
    echo "GitLab CI Lint call failed (glab or token missing?)"
    return 1
  }
  echo "$out" | jq -c '{valid, errors, warnings}'
  [ "$(echo "$out" | jq -r .valid)" = true ]
}

check() {
  local gitlab=0 fail=0 workflows=()
  case ${1:-} in
    "") ;;
    --gitlab) gitlab=1 ;;
    *) echo "unknown argument: $1" >&2; return 2 ;;
  esac
  cd "$(git rev-parse --show-toplevel)" || return 2

  shopt -s nullglob
  workflows=(.github/workflows/*.yml .github/workflows/*.yaml)
  shopt -u nullglob

  echo "── yaml parse: ${workflows[*]} .gitlab-ci.yml"
  yaml_parses "${workflows[@]}" .gitlab-ci.yml || fail=1

  # actionlint takes workflow files (not a directory); zizmor accepts either.
  for tool in actionlint zizmor; do
    if command -v "$tool" >/dev/null; then
      echo "── $tool"
      "$tool" "${workflows[@]}" || fail=1
    else
      echo "SKIP $tool (not installed)"
    fi
  done

  echo "── pins: rust-toolchain.toml's channel everywhere, each repeated tool version once"
  pins || fail=1

  if [ "$gitlab" = 1 ]; then
    echo "── GitLab CI Lint: .gitlab-ci.yml"
    gitlab_lint || fail=1
  else
    echo "SKIP GitLab CI Lint (pass --gitlab; needs glab and a token)"
  fi
  return $fail
}

case ${1:-} in
  only) shift; only "$@" ;;
  check) shift; check "$@" ;;
  *) echo "usage: ci-config-check.sh only <base-ref> | check [--gitlab]" >&2; exit 2 ;;
esac
