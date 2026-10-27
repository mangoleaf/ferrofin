#!/usr/bin/env bash
# Functions shared by gates/verify/scan-behaviour.sh and gates/verify/run.sh. Sourced, never executed:
# nothing here talks to a server or touches media (verify_combine writes verify.json in the run's folder
# and removes the rows files it gathers),
# which is what lets gates/verify/tests/*.bats drive every piece of logic with canned /metrics text and
# throwaway files.

# The item names every scratch item carries (from its NFO), so the script finds its own items
# and nothing else.
# shellcheck disable=SC2034 # used by scan-behaviour.sh
VERIFY_PREFIX='Ferrofin Verify'
# A 1x1 PNG: the local poster row 11 adds.
# shellcheck disable=SC2034 # used by scan-behaviour.sh
VERIFY_POSTER_PNG='iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP438AAAAQBAYDFKhhdAAAAAElFTkSuQmCC'

# verify_metric_sum <scrape-file> <metric> [label=value ...]: the sum of every series of
# <metric> that carries all the given labels, as an integer (0 when there is none).
verify_metric_sum() {
  local file=$1 name=$2
  shift 2
  awk -v name="$name" -v want="$*" '
    BEGIN { n = split(want, labels, " ") }
    index($0, name "{") == 1 || index($0, name " ") == 1 {
      ok = 1
      for (i = 1; i <= n; i++) {
        eq = index(labels[i], "=")
        needle = substr(labels[i], 1, eq - 1) "=\"" substr(labels[i], eq + 1) "\""
        if (index($0, needle) == 0) ok = 0
      }
      if (ok) sum += $NF
    }
    END { printf "%d\n", sum + 0 }' "$file"
}

# verify_delta <before-file> <after-file> <metric> [label=value ...]: how much the summed series
# grew between two scrapes.
verify_delta() {
  local before=$1 after=$2
  shift 2
  echo $(( $(verify_metric_sum "$after" "$@") - $(verify_metric_sum "$before" "$@") ))
}

# verify_metrics_enabled <scrape-file> <http-status>: a reason when the scrape is not a Ferrofin
# /metrics with the library-scan metrics in it, nothing when it is.
verify_metrics_enabled() {
  local file=$1 status=$2
  if [ "$status" = 404 ]; then
    echo "/metrics is disabled (404): start the server with FERROFIN_ENABLE_METRICS=true (or EnableMetrics in system.json) and restart it"
  elif [ "$status" != 200 ]; then
    echo "/metrics answered HTTP $status"
  elif ! grep -q '^ferrofin_library_scans_total{' "$file"; then
    echo "/metrics has no ferrofin_library_scans_total: this server predates the library-scan metrics"
  fi
}

# verify_inside <path> <location>...: success when <path> is strictly below one of the library
# locations (never a location itself). A path with a '.' or '..' segment is refused outright: it
# could name a folder outside the library while reading as one inside it. (Symlinks on the
# local side are the script's own check, against realpath; this one also runs on the server's
# view of the path, which need not exist here.)
verify_inside() {
  local path=${1%/} location
  shift
  case /$path/ in */./*|*/../*) return 1 ;; esac
  for location in "$@"; do
    location=${location%/}
    case $path in "$location"/?*) return 0 ;; esac
  done
  return 1
}

# verify_nfo <kind> <title> <plot> [trailer]: a Kodi NFO for a movie, tvshow or episodedetails
# with everything the scan's "already enriched" gates look for, so an item no remote provider
# can match is not asked about again on every scan (the backfill heuristics): a title and a
# plot, a community rating and a critic rating (the gates OMDb adds when it is on for the
# type: TMDb's asks for a critic rating, OMDb's alone for an overview and both ratings), an
# official rating (the field row 7 clears and row 8 fills, which no gate reads), and
# optionally a YouTube trailer (TMDb's gate for a movie or series).
verify_nfo() {
  local kind=$1 title=$2 plot=$3 trailer=${4:-}
  printf '<?xml version="1.0" encoding="utf-8"?>\n<%s>\n' "$kind"
  printf '  <title>%s</title>\n  <plot>%s</plot>\n' "$title" "$plot"
  printf '  <rating>6.5</rating>\n  <criticrating>70</criticrating>\n  <mpaa>PG</mpaa>\n'
  [ -z "$trailer" ] || printf '  <trailer>plugin://plugin.video.youtube/play/?video_id=%s</trailer>\n' "$trailer"
  printf '</%s>\n' "$kind"
}

# VERIFY_CHECKS: every check since it was last emptied (a row's start), one JSON object per line,
# {"what", "observed", "op", "expected", "ok"}: what verify_expect, verify_expect_str and verify_fail
# were asked and found, passed or not, so a row's report can show each check's observed and expected
# value. An integer check's values are numbers, a string check's strings (op "eq"), and a failure
# no comparison stands behind has null values.
VERIFY_CHECKS=()

# verify_check <what> <observed> <op> <expected> <ok> [int]: one entry of VERIFY_CHECKS. With int,
# <observed> and <expected> become numbers (decimal, leading zeros included), else they stay strings;
# an empty <op> makes all three null.
verify_check() {
  VERIFY_CHECKS+=("$(jq -nc --arg what "$1" --arg observed "$2" --arg op "$3" --arg expected "$4" --argjson ok "$5" \
    --arg kind "${6:-}" '
    def value: if $kind == "int" and test("^-?[0-9]+$") then tonumber else . end;
    {what: $what, observed: (if $op == "" then null else ($observed | value) end),
     op: (if $op == "" then null else $op end),
     expected: (if $op == "" then null else ($expected | value) end), ok: $ok}')")
}

# verify_expect <failures-var> <what> <actual> <op> <expected>: appends "<what> <actual> (want
# <op> <expected>)" to the named array when the integer comparison does not hold, and the check
# to VERIFY_CHECKS either way. <op> is one of eq, le, ge.
verify_expect() {
  # shellcheck disable=SC2178 # a nameref to the caller's array
  local -n _fails=$1
  local what=$2 actual=$3 op=$4 expected=$5 ok=1
  case $op in
    eq) [ "$actual" -eq "$expected" ] || ok=0 ;;
    le) [ "$actual" -le "$expected" ] || ok=0 ;;
    ge) [ "$actual" -ge "$expected" ] || ok=0 ;;
    *) ok=0 ;;
  esac
  if [ "$ok" = 0 ]; then
    local sign
    case $op in eq) sign='=' ;; le) sign='<=' ;; ge) sign='>=' ;; *) sign=$op ;; esac
    _fails+=("$what $actual (want $sign $expected)")
    verify_check "$what" "$actual" "$op" "$expected" false int
  else
    verify_check "$what" "$actual" "$op" "$expected" true int
  fi
}

# verify_expect_str <failures-var> <what> <actual> <expected>: the string form of verify_expect.
verify_expect_str() {
  # shellcheck disable=SC2178 # a nameref to the caller's array
  local -n _fails=$1
  if [ "$3" = "$4" ]; then
    verify_check "$2" "$3" eq "$4" true
  else
    _fails+=("$2 '$3' (want '$4')")
    verify_check "$2" "$3" eq "$4" false
  fi
}

# verify_fail <failures-var> <what>: a failure no comparison stands behind (a refused request, an
# item missing from /Items), in the named array and in VERIFY_CHECKS.
verify_fail() {
  # shellcheck disable=SC2178 # a nameref to the caller's array
  local -n _fails=$1
  _fails+=("$2")
  verify_check "$2" '' '' '' false
}

# verify_row_line <row> <result> <scenario> <detail>: one line of the PASS/FAIL table.
verify_row_line() {
  printf '%4s  %-4s  %-46s  %s\n' "$1" "$2" "$3" "$4"
}

# verify_finish <dir> <exit status> <image> <image id> <commit> <started>: what gates/verify/run.sh
# leaves however its run ends: DIR/verify.json (every library null when the run ended before
# verify_combine wrote it), then DIR/result.json and DIR/report.html from it (verify_report.py).
# Fails, naming what, when the result or the report could not be written.
verify_finish() {
  local dir=$1 rc=$2
  [ -e "$dir/verify.json" ] || verify_combine "$dir" "$rc" movies tvshows
  python3 "$(dirname "${BASH_SOURCE[0]}")/verify_report.py" "$dir" --image "$3" --image-id "$4" --commit "$5" \
    --started "$6" >/dev/null || { echo "verify: the result or the report could not be written in $dir" >&2; return 1; }
}

# verify_combine <dir> <exit status> <library kind…>: DIR/verify.json, what gates/report.py reads: the
# exit status and, per library, the rows scan-behaviour.sh --json wrote to DIR/rows-<kind>.json (null
# when that file is missing or unreadable: the check ended before its rows), which it then removes.
verify_combine() {
  local dir=$1 rc=$2 kind rows libraries='{}'
  shift 2
  for kind; do
    rows=$(jq -c . "$dir/rows-$kind.json" 2>/dev/null) || rows=''
    [ -n "$rows" ] || rows=null
    libraries=$(jq -c --arg kind "$kind" --argjson rows "$rows" '. + {($kind): $rows}' <<<"$libraries")
    rm -f -- "$dir/rows-$kind.json"
  done
  jq -n --argjson exit "$rc" --argjson libraries "$libraries" '{exit: $exit, libraries: $libraries}' > "$dir/verify.json"
}
