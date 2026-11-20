#!/usr/bin/env bats
# Tests for gates/isolation.sh, the isolation steps gates/bench/run.sh and gates/parity/run.sh share: the
# plugin policy (ALL=1 for bench's Ferrofin, ALL=0 otherwise), the checks that the isolation holds, and the
# wait for idle tasks. `api` and `api_post` are stubs over canned JSON, so no server is needed.
# shellcheck disable=SC2030,SC2031  # each test reads FAIL_REASON in its own shell, where it set it

bats_require_minimum_version 1.5.0

setup() {
  # shellcheck source=gates/isolation.sh
  . "$BATS_TEST_DIRNAME/../isolation.sh"
  D="$BATS_TEST_TMPDIR/server"
  CANNED="$BATS_TEST_TMPDIR/canned"
  mkdir -p "$D" "$CANNED/Library"
  DRAIN_TIMEOUT_S=2
  # Two plugins that can be uninstalled and one in-tree one, which Jellyfin cannot disable.
  echo '[{"Id":"a","Version":"1","CanUninstall":false,"Status":"Active"},
         {"Id":"b","Version":"2","CanUninstall":true,"Status":"Active"},
         {"Id":"c","Version":"3","Status":"Active"}]' > "$CANNED/Plugins"
  echo '[{"Id":"t1","Name":"Scan","State":"Idle","Triggers":[]},{"Id":"t2","Name":"Clean","State":"Idle","Triggers":[]}]' > "$CANNED/ScheduledTasks"
}

api() { cat "$CANNED/${1#/}"; }   # api PATH: the canned answer for PATH
api_post() { echo "$1 ${2:-}" >> "$BATS_TEST_TMPDIR/posts"; }

posts() { cat "$BATS_TEST_TMPDIR/posts" 2>/dev/null; }
fails() { if "$@"; then return 1; fi; }   # fails CMD…: CMD returns non-zero, run in this shell so FAIL_REASON stays readable

@test "isolation_disable_plugins: ALL=0 leaves an in-tree plugin alone, ALL=1 disables every one" {
  isolation_disable_plugins 0
  [ "$(posts)" = "$(printf '/Plugins/b/2/Disable \n/Plugins/c/3/Disable ')" ]
  rm "$BATS_TEST_TMPDIR/posts"
  isolation_disable_plugins 1
  [ "$(posts)" = "$(printf '/Plugins/a/1/Disable \n/Plugins/b/2/Disable \n/Plugins/c/3/Disable ')" ]
  cmp -s "$CANNED/Plugins" "$D/plugins-before.json"
}

@test "isolation_disable_plugins: an answer that is not a list is a failure with its reason" {
  echo '{}' > "$CANNED/Plugins"
  fails isolation_disable_plugins 0
  [ "$FAIL_REASON" = "cannot enumerate plugins" ]
  [ -z "$(posts)" ]
}

@test "isolation_disable_triggers: every task's triggers are emptied" {
  isolation_disable_triggers
  [ "$(posts)" = "$(printf '/ScheduledTasks/t1/Triggers []\n/ScheduledTasks/t2/Triggers []')" ]
}

@test "isolation_verify: an active in-tree plugin passes ALL=0 and fails ALL=1; a trigger or a monitored library fails both" {
  echo '[{"CanUninstall":false,"Status":"Active"},{"CanUninstall":true,"Status":"Disabled"}]' > "$CANNED/Plugins"
  echo '[{"LibraryOptions":{"EnableRealtimeMonitor":false,"AutomaticRefreshIntervalDays":0,"TypeOptions":[{"MetadataFetchers":[],"ImageFetchers":[]}]}}]' > "$CANNED/Library/VirtualFolders"
  isolation_verify 0
  FAIL_REASON=""
  fails isolation_verify 1
  [ "$FAIL_REASON" = "plugins not disabled" ]

  echo '[{"Id":"t1","Name":"Scan","State":"Idle","Triggers":[{"Type":"StartupTrigger"}]}]' > "$CANNED/ScheduledTasks"
  fails isolation_verify 0
  [ "$FAIL_REASON" = "automatic task triggers remain enabled" ]

  echo '[{"Id":"t1","Name":"Scan","State":"Idle","Triggers":[]}]' > "$CANNED/ScheduledTasks"
  local library
  for library in '"EnableRealtimeMonitor":true,"AutomaticRefreshIntervalDays":0,"TypeOptions":[{"MetadataFetchers":[],"ImageFetchers":[]}]' \
    '"EnableRealtimeMonitor":false,"AutomaticRefreshIntervalDays":7,"TypeOptions":[{"MetadataFetchers":[],"ImageFetchers":[]}]' \
    '"EnableRealtimeMonitor":false,"AutomaticRefreshIntervalDays":0,"TypeOptions":[{"MetadataFetchers":["TheMovieDb"],"ImageFetchers":[]}]' \
    '"EnableRealtimeMonitor":false,"AutomaticRefreshIntervalDays":0,"TypeOptions":[{"MetadataFetchers":[],"ImageFetchers":["TheMovieDb"]}]' \
    '"EnableRealtimeMonitor":false,"AutomaticRefreshIntervalDays":0,"TypeOptions":[]'; do
    echo "[{\"LibraryOptions\":{$library}}]" > "$CANNED/Library/VirtualFolders"
    FAIL_REASON=""
    fails isolation_verify 0
    [ "$FAIL_REASON" = "library monitoring, automatic refresh or fetchers not disabled" ]
  done
  echo '[]' > "$CANNED/Library/VirtualFolders"   # no library at all is no evidence either
  fails isolation_verify 0
}

@test "isolation_tasks_idle: idle tasks pass at once; a task still running past the deadline fails, naming it" {
  isolation_tasks_idle
  echo '[{"Id":"t1","Name":"Scan","State":"Running","Triggers":[]}]' > "$CANNED/ScheduledTasks"
  NAME=ferrofin fails isolation_tasks_idle > "$BATS_TEST_TMPDIR/drain.out"
  [ "$FAIL_REASON" = "scheduled tasks did not go idle within 2s" ]
  grep -q "drain: ferrofin waiting for \[Scan\]" "$BATS_TEST_TMPDIR/drain.out"
  echo '{}' > "$CANNED/ScheduledTasks"   # an answer that is not a list is not a timeout
  fails isolation_tasks_idle
  [ "$FAIL_REASON" = "cannot read scheduled tasks" ]
}

@test "isolation_no_wasm and isolation_realtime_monitor_off: a WASM file is refused; every library's monitor is switched off" {
  mkdir -p "$BATS_TEST_TMPDIR/config/root/default/Movies" "$BATS_TEST_TMPDIR/config/root/default/Shows" "$BATS_TEST_TMPDIR/config/plugins"
  isolation_no_wasm "$BATS_TEST_TMPDIR/config"
  touch "$BATS_TEST_TMPDIR/config/plugins/x.wasm"
  fails isolation_no_wasm "$BATS_TEST_TMPDIR/config"
  [[ "$FAIL_REASON" == *"WASM plugin"* ]]

  fails isolation_realtime_monitor_off "$BATS_TEST_TMPDIR/config" 2>/dev/null   # no options.xml yet
  echo '<LibraryOptions><EnableRealtimeMonitor>true</EnableRealtimeMonitor></LibraryOptions>' > "$BATS_TEST_TMPDIR/config/root/default/Movies/options.xml"
  echo '<LibraryOptions><Enabled>true</Enabled></LibraryOptions>' > "$BATS_TEST_TMPDIR/config/root/default/Shows/options.xml"   # no element yet
  isolation_realtime_monitor_off "$BATS_TEST_TMPDIR/config"
  grep -q '<EnableRealtimeMonitor>false</EnableRealtimeMonitor>' "$BATS_TEST_TMPDIR/config/root/default/Movies/options.xml"
  grep -q '<EnableRealtimeMonitor>false</EnableRealtimeMonitor>' "$BATS_TEST_TMPDIR/config/root/default/Shows/options.xml"
}
