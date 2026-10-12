#!/usr/bin/env bash
# Functions shared by verify/scan-behaviour.sh. Sourced, never executed: nothing here talks to a
# server or touches media, which is what lets verify/tests/*.bats drive every piece of logic
# with canned /metrics text and throwaway files.

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

# verify_expect <failures-var> <what> <actual> <op> <expected>: appends "<what> <actual> (want
# <op> <expected>)" to the named array when the integer comparison does not hold. <op> is one of
# eq, le, ge.
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
  fi
}

# verify_expect_str <failures-var> <what> <actual> <expected>: the string form of verify_expect.
verify_expect_str() {
  # shellcheck disable=SC2178 # a nameref to the caller's array
  local -n _fails=$1
  [ "$3" = "$4" ] || _fails+=("$2 '$3' (want '$4')")
}

# verify_row_line <row> <result> <scenario> <detail>: one line of the PASS/FAIL table.
verify_row_line() {
  printf '%4s  %-4s  %-46s  %s\n' "$1" "$2" "$3" "$4"
}
