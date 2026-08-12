#!/usr/bin/env bash
# Generate Ferrofin's version strings from git tags — the single source of truth
# for both the release tag (create-release) and the image tag (docker build).
# Kept here (not inline in .gitlab-ci.yml) so the logic is unit-testable with bats
# and checkable with shellcheck instead of buried in YAML.
#
# Subcommands:
#   next    Print the NEXT release version (vX.Y.Z) from the latest tag plus the
#           Conventional Commit subjects since it:
#             feat -> minor ; fix/anything else -> patch ; type!: -> breaking.
#           While the major is 0, a breaking change bumps MINOR (never auto-1.0.0).
#           FORCE_VERSION=vX.Y.Z overrides the computed value.
#   image   Print the image/build version. On a release-tag pipeline
#           (CI_COMMIT_TAG set) that's the tag verbatim, e.g. v0.5.2 (keeps the
#           leading v to match the git tag); otherwise a dev version
#           v{Major}.{Minor}.{Patch}-{N}-{sha12}, where N counts commits since the
#           latest tag that touched build-relevant paths.
#
# Reads only git state + the CI_COMMIT_TAG / FORCE_VERSION env vars, so it runs
# identically in CI and in a bats test against a throwaway repo.
set -euo pipefail

# A commit touching any of these warrants a new image (drives the `image` count).
BUILD_PATHS=(crates apps Cargo.toml Cargo.lock Dockerfile)

# Highest vX.Y.Z release tag, or empty if none. `sed -n 1p` reads to EOF so the
# upstream git/grep never write to a closed pipe (a `| head -1` there SIGPIPEs → 141).
latest_tag() {
  git tag -l --sort=-v:refname 'v[0-9]*' \
    | { grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' || true; } \
    | sed -n 1p
}

# Split "X.Y.Z" (leading v optional) into the globals MAJOR MINOR PATCH.
parse_semver() {
  local v=${1#v}
  MAJOR=${v%%.*}
  local rest=${v#*.}
  MINOR=${rest%%.*}
  PATCH=${rest##*.}
}

next_version() {
  if [ -n "${FORCE_VERSION:-}" ]; then
    printf '%s\n' "$FORCE_VERSION"
    return
  fi
  local latest subjects bang feat level
  latest=$(latest_tag)
  if [ -n "$latest" ]; then
    subjects=$(git log --format='%s' "${latest}..HEAD")
    parse_semver "$latest"
  else
    subjects=$(git log --format='%s')
    parse_semver "0.0.0"
  fi
  # SUBJECT line only — body prose (e.g. docs mentioning "BREAKING CHANGE") must
  # not sway the version. grep -c reads all input (no early-exit SIGPIPE).
  bang=$(printf '%s\n' "$subjects" | grep -Ec '^[a-z]+(\([^)]*\))?!:' || true)
  feat=$(printf '%s\n' "$subjects" | grep -Ec '^feat(\([^)]*\))?:' || true)
  if [ "$bang" -gt 0 ]; then
    if [ "$MAJOR" -eq 0 ]; then level="minor"; else level="major"; fi
  elif [ "$feat" -gt 0 ]; then
    level="minor"
  else
    level="patch"
  fi
  case "$level" in
    major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
    minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
    patch) PATCH=$((PATCH + 1)) ;;
  esac
  printf 'v%s.%s.%s\n' "$MAJOR" "$MINOR" "$PATCH"
}

image_version() {
  if [ -n "${CI_COMMIT_TAG:-}" ]; then
    printf '%s\n' "$CI_COMMIT_TAG"          # keep the leading v, e.g. v0.5.2
    return
  fi
  local latest base range count sha
  latest=$(latest_tag)
  if [ -n "$latest" ]; then
    base=$latest                            # keep the leading v
    range="${latest}..HEAD"
  else
    base="v0.0.0"
    range="HEAD"
  fi
  count=$(git rev-list --count "$range" -- "${BUILD_PATHS[@]}")
  sha=$(git rev-parse --short=12 HEAD)
  printf '%s-%s-%s\n' "$base" "$count" "$sha"
}

main() {
  case "${1:-}" in
    next) next_version ;;
    image) image_version ;;
    *)
      echo "usage: $0 {next|image}" >&2
      exit 2
      ;;
  esac
}

main "$@"
