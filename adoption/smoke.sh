#!/usr/bin/env bash
# smoke.sh <base-url> <data-dir> [username]
#
# 38 read-only probes against a server running on an adopted Jellyfin database. Prints
# "status  key-metric  path" per probe so two servers on the same library can be diffed.
# Credentials are read from the given copy's own jellyfin.db at run time (the newest admin
# API key and a web-session token of the chosen user) and are never written anywhere. The
# user defaults to the administrator with the most recently active "Jellyfin Web" session
# (the same account on every copy of one library); pass a username to pin it.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=adoption/lib.sh
. "$HERE/lib.sh"
BASE=${1:?base url}; DB=${2:?data dir}/data/jellyfin.db; WANT_USER=${3:-${ADOPTION_USER:-}}
q() { sqlite3 -readonly "file:$DB?mode=ro" "$1"; }
APIKEY=$(q 'SELECT AccessToken FROM ApiKeys ORDER BY DateCreated DESC LIMIT 1')
read -r _ USER_TOKEN USER_DEVICE USER_ID < <(adoption_smoke_credentials "$DB" "$WANT_USER") || exit 2
ADMIN="Authorization: MediaBrowser Token=\"$APIKEY\", Client=\"smoke\", Device=\"smoke\", DeviceId=\"smoke\", Version=\"1\""
USERH="Authorization: MediaBrowser Token=\"$USER_TOKEN\", Client=\"Jellyfin Web\", Device=\"Chrome\", DeviceId=\"$USER_DEVICE\", Version=\"10.11.8\""
hit() { # hit <auth-header|-> <jq-expr> <path>
  local auth=$1 jq=$2 path=$3 tmp code metric; tmp=$(mktemp)
  if [ "$auth" = - ]; then code=$(curl -s -o "$tmp" -w '%{http_code}' "$BASE$path"); else code=$(curl -s -o "$tmp" -w '%{http_code}' -H "$auth" "$BASE$path"); fi
  metric=$(jq -r "$jq" "$tmp" 2>/dev/null | head -c 90 | tr '\n' ' ')
  [ -n "$metric" ] || metric="(bytes=$(wc -c <"$tmp"))"
  printf '%s  %-45s %s\n' "$code" "$metric" "$path"; rm -f "$tmp"
}
hit - '.Version+" "+.ProductName' /System/Info/Public
hit "$ADMIN" '.Version+" pendingRestart="+(.HasPendingRestart|tostring)' /System/Info
hit "$ADMIN" '[.[].Name]|join(",")' /Users
hit "$USERH" '.Name+" admin="+(.Policy.IsAdministrator|tostring)' /Users/Me
hit "$USERH" '[.Items[].Name]|join(",")' /Library/MediaFolders
hit "$USERH" '[.Items[]|.Name]|join(",")' "/Users/$USER_ID/Views"
for t in Movie Series Season Episode Audio MusicAlbum BoxSet Book Photo; do hit "$USERH" '.TotalRecordCount' "/Items?UserId=$USER_ID&Recursive=true&IncludeItemTypes=$t&Limit=0"; done
hit "$USERH" '.TotalRecordCount' "/Items?UserId=$USER_ID&Recursive=true&IncludeItemTypes=Movie,Episode&Filters=IsPlayed&Limit=0"
hit "$USERH" '.TotalRecordCount' "/Items?UserId=$USER_ID&Recursive=true&Filters=IsFavorite&Limit=0"
hit "$USERH" '(.Items|length|tostring)+" first="+(.Items[0].Name//"-")' "/Users/$USER_ID/Items/Resume?Limit=12&MediaTypes=Video"
hit "$USERH" '(.Items|length|tostring)+" first="+(.Items[0].SeriesName//"-")' "/Shows/NextUp?UserId=$USER_ID&Limit=12"
hit "$USERH" '(.|length|tostring)+" first="+(.[0].Name//"-")' "/Users/$USER_ID/Items/Latest?Limit=12&IncludeItemTypes=Movie"
hit "$USERH" '.TotalRecordCount' "/Persons?UserId=$USER_ID&Limit=0"
hit "$USERH" '.TotalRecordCount' "/Genres?UserId=$USER_ID&Limit=0"
hit "$USERH" '.TotalRecordCount' "/Artists?UserId=$USER_ID&Limit=0"
hit "$USERH" '.TotalRecordCount' "/Studios?UserId=$USER_ID&Limit=0"
hit "$USERH" '.TotalRecordCount' "/Items?UserId=$USER_ID&Recursive=true&IncludeItemTypes=Playlist&Limit=0"
hit "$USERH" '(.Items|length)' "/Items?UserId=$USER_ID&Recursive=true&SearchTerm=the&Limit=5"
hit "$ADMIN" 'length' /Sessions
hit "$ADMIN" '(length|tostring)+" running="+([.[]|select(.State=="Running")|.Name]|join(","))' /ScheduledTasks
hit "$ADMIN" '[.[]|.Name]|join(",")' /Plugins
hit "$ADMIN" '.TotalRecordCount' /System/ActivityLog/Entries?Limit=1
hit "$ADMIN" '.TotalRecordCount' /LiveTv/Channels
hit "$ADMIN" '.TotalRecordCount' /Devices
hit "$ADMIN" '[.[]|.Name]|join(",")' /Library/VirtualFolders
# one deterministic movie: details, playback info, primary image
MOVIE=$(curl -s -H "$USERH" "$BASE/Items?UserId=$USER_ID&Recursive=true&IncludeItemTypes=Movie&Limit=1&SortBy=SortName&SortOrder=Ascending" | jq -r '.Items[0].Id')
hit "$USERH" '.Name+" streams="+(.MediaStreams|length|tostring)+" played="+(.UserData.Played|tostring)' "/Users/$USER_ID/Items/$MOVIE"
hit "$USERH" '(.MediaSources|length|tostring)+" src directPlay="+(.MediaSources[0].SupportsDirectPlay|tostring)' "/Items/$MOVIE/PlaybackInfo?UserId=$USER_ID"
hit - '"(image)"' "/Items/$MOVIE/Images/Primary?maxWidth=200"
hit - '"(web)"' /web/index.html
