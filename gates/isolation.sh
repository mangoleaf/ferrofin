# How gates/bench/run.sh and gates/parity/run.sh put a server in the isolated state they measure, and wait for
# it: one definition, so the two harnesses cannot drift apart (gl-754). Each harness keeps its own timing (the
# benchmark's settle once the tasks are idle, the parity run's per-server sign-in) and its own plugin policy.
#
# Sourced from the repository root; callers set `set -euo pipefail` themselves. The caller defines `api PATH`
# (an authenticated GET of $URL$PATH) and `api_post PATH [BODY]` (an authenticated JSON POST), and sets URL, D
# (the server's run directory), READY_TIMEOUT_S, HTTP_TIMEOUT_S and DRAIN_TIMEOUT_S. A step that fails returns 1
# with its reason in FAIL_REASON; the caller decides what that ends.
# shellcheck shell=bash disable=SC2034  # FAIL_REASON is the caller's to read

POLL_S=1   # between readiness and drain polls

ping_ready() {   # the server at $URL answers /System/Ping and reports a version
  curl -sf --connect-timeout "$HTTP_TIMEOUT_S" --max-time "$HTTP_TIMEOUT_S" "$URL/System/Ping" >/dev/null 2>&1 &&
    curl -sf --connect-timeout "$HTTP_TIMEOUT_S" --max-time "$HTTP_TIMEOUT_S" "$URL/System/Info/Public" 2>/dev/null | jq -e '.Version | strings | length > 0' >/dev/null 2>&1
}

wait_ready() {
  local give_up=$((SECONDS + READY_TIMEOUT_S))
  while (( SECONDS < give_up )); do ping_ready && return 0; sleep "$POLL_S"; done
  FAIL_REASON="readiness timed out"; return 1
}

isolation_no_wasm() {   # isolation_no_wasm CONFIG: no WASM plugin rides along in the copy; the answers are the core server's
  local wasm
  wasm=$(find "$1" -name '*.wasm' -print -quit 2>/dev/null)
  [ -z "$wasm" ] || { FAIL_REASON="a WASM plugin is present in the configuration copy ($wasm)"; return 1; }
}

isolation_realtime_monitor_off() {   # isolation_realtime_monitor_off CONFIG: in the disposable copy, before the server starts its watcher
  python3 - "$1" <<'PY_ISOLATION' || { FAIL_REASON="no library options in the configuration copy"; return 1; }
import pathlib, sys, xml.etree.ElementTree as ET
files = list(pathlib.Path(sys.argv[1]).glob("root/default/*/options.xml"))
if not files:
    raise SystemExit("no library options found in the configuration")
for path in files:
    tree = ET.parse(path)
    root = tree.getroot()
    node = root.find("EnableRealtimeMonitor")
    if node is None:
        node = ET.SubElement(root, "EnableRealtimeMonitor")
    node.text = "false"
    tree.write(path, encoding="utf-8", xml_declaration=True)
PY_ISOLATION
}

# isolation_disable_plugins ALL: ALL=1 disables every plugin; ALL=0 leaves one that cannot be uninstalled (an
# in-tree one, which Jellyfin cannot disable) as it is. Jellyfin unloads a disabled plugin only on restart.
isolation_disable_plugins() {
  local all=$1 id version
  if ! api /Plugins > "$D/plugins-before.json" || ! jq -e 'type == "array"' "$D/plugins-before.json" >/dev/null; then
    FAIL_REASON="cannot enumerate plugins"; return 1
  fi
  while read -r id version; do
    api_post "/Plugins/$id/$version/Disable" || { FAIL_REASON="could not disable plugin $id $version"; return 1; }
  done < <(jq -r --argjson all "$all" '.[] | select($all == 1 or .CanUninstall != false) | "\(.Id) \(.Version)"' "$D/plugins-before.json")
}

isolation_disable_triggers() {   # every scheduled task's triggers emptied, startup triggers included: no automatic task runs
  local id
  if ! api /ScheduledTasks > "$D/tasks-before.json" || ! jq -e 'type == "array" and length > 0' "$D/tasks-before.json" >/dev/null; then
    FAIL_REASON="cannot read scheduled tasks"; return 1
  fi
  while read -r id; do
    api_post "/ScheduledTasks/$id/Triggers" '[]' || { FAIL_REASON="could not clear the triggers of task $id"; return 1; }
  done < <(jq -r '.[].Id' "$D/tasks-before.json")
}

isolation_require() {   # isolation_require FILE PATH MESSAGE FILTER [jq args]: fetch PATH into FILE and demand the jq FILTER holds of it
  local file=$1 path=$2 message=$3 filter=$4
  shift 4
  if ! api "$path" > "$file" || ! jq -e "$@" "$filter" "$file" >/dev/null; then FAIL_REASON=$message; return 1; fi
}

isolation_verify() {   # isolation_verify ALL: the state the steps above leave is still in place (ALL as isolation_disable_plugins)
  # shellcheck disable=SC2016  # $all is jq's
  isolation_require "$D/plugins.json" /Plugins "plugins not disabled" \
    'type == "array" and all(.[]; .Status == "Disabled" or ($all == 0 and .CanUninstall == false))' --argjson all "$1" || return 1
  isolation_require "$D/tasks.json" /ScheduledTasks "automatic task triggers remain enabled" \
    'type == "array" and length > 0 and all(.[]; .Triggers == [])' || return 1
  isolation_require "$D/libraries.json" /Library/VirtualFolders "library monitoring, automatic refresh or fetchers not disabled" \
    'type == "array" and length > 0 and all(.[]; .LibraryOptions | .EnableRealtimeMonitor == false and .AutomaticRefreshIntervalDays == 0 and (.TypeOptions | length > 0 and all(.[]; .MetadataFetchers == [] and .ImageFetchers == [])))'
}

isolation_tasks_idle() {   # every scheduled task idle within DRAIN_TIMEOUT_S
  local give_up=$((SECONDS + DRAIN_TIMEOUT_S)) busy
  while (( SECONDS < give_up )); do
    busy=$(api /ScheduledTasks | jq -er 'if type != "array" then error("invalid tasks") else [.[] | select(.State != "Idle") | .Name] | join(",") end') ||
      { FAIL_REASON="cannot read scheduled tasks"; return 1; }
    [ -n "$busy" ] || return 0
    echo "  drain: ${NAME:+$NAME }waiting for [$busy]"   # parity boots both servers at once
    sleep "$POLL_S"
  done
  FAIL_REASON="scheduled tasks did not go idle within ${DRAIN_TIMEOUT_S}s"; return 1
}
