#!/usr/bin/env bash
# The per-crate 80% line-coverage gate, shared by the GitHub and GitLab pipelines so
# the rule lives in one place (and is shellcheck-clean and bats-tested, not buried in YAML).
#
#   coverage-gate.sh <core|heavy|rest>
#
# The crates are split into three shards so the pipelines can run them in parallel:
#   core   ferrofin-core
#   heavy  ferrofin-api ferrofin-wasm ferrofin-livetv ferrofin-extensions
#   rest   every other crate in crates/ (computed, so a new crate can never skip the gate)
# ferrofin-traits is exempt (trait definitions; the bodies live in ferrofin-core — see
# CLAUDE.md), as is apps/ferrofin-server (the composition root).
#
# Gate EACH crate independently: `-p a -p b --fail-under-lines` checks the MERGED total,
# which lets a sub-80% crate hide behind a stronger sibling. Every crate in the shard runs
# and the script fails at the end, so one failing crate does not hide another. Line
# coverage only: branch coverage (-Z coverage-options=branch) is nightly-only; we pin stable.
#
# Exit codes: 0 all gated crates passed; 1 a crate failed its tests or the gate;
# 2 usage error or a crate named in `core`/`heavy` has no directory (a rename would
# otherwise silently drop it from the gate).
set -uo pipefail

CORE=(ferrofin-core)
HEAVY=(ferrofin-api ferrofin-wasm ferrofin-livetv ferrofin-extensions)
EXEMPT=ferrofin-traits

# Always relative to the repo root, wherever the caller's working directory is.
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 2

shard=${1:-}
mapfile -t all < <(basename -a crates/*/)

for named in "${CORE[@]}" "${HEAVY[@]}"; do
  if [ ! -d "crates/$named" ]; then
    echo "coverage-gate: crates/$named is named in a shard but does not exist" >&2
    exit 2
  fi
done

case $shard in
  core) crates=("${CORE[@]}") ;;
  heavy) crates=("${HEAVY[@]}") ;;
  rest)
    crates=()
    for c in "${all[@]}"; do
      case " ${CORE[*]} ${HEAVY[*]} $EXEMPT " in *" $c "*) ;; *) crates+=("$c") ;; esac
    done
    ;;
  *)
    echo "usage: coverage-gate.sh <core|heavy|rest>" >&2
    exit 2
    ;;
esac

fail=0
for crate in "${crates[@]}"; do
  echo "── coverage ($shard): $crate ──"
  # `-p X` still instruments X's workspace deps, so their (uncovered) source lands in
  # the TOTAL the gate compares against. Exclude every sibling.
  others=$(printf '%s\n' "${all[@]}" | grep -vx "$crate" | paste -sd'|')
  if ! cargo llvm-cov nextest -p "$crate" --locked --fail-under-lines 80 --summary-only \
    --ignore-filename-regex "(crates/($others)|apps)/"; then
    # GitHub turns this prefix into an annotation; elsewhere it is a plain line.
    echo "::error::Tests or coverage failed for $crate"
    fail=1
  fi
done
exit $fail
