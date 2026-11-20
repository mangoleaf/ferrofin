#!/usr/bin/env bash
# The round trip's routes, sourced by run.sh: route|fixture directory|the generation Ferrofin adopts it
# as (the boot log's, as in gates/adoption/run.sh's FIXTURE_TABLE)|the fixture whose build log
# (<fixture>.server.log) is that Jellyfin release's own first start on the route's source: the code
# migrations it ran are the ones Jellyfin must run on the export|the data directory Ferrofin runs at, and
# the export reads it at. Each fixture is one gates/adoption/build-fixtures.sh builds. A route is named for
# its source, and every route returns to the same Jellyfin release (rt_jellyfin_image), always at /config:
# a route whose Ferrofin runs at /data (its own image's directory) is one whose stored paths the export must
# rewrite. More routes come with the export's other generations.
# shellcheck disable=SC2034  # read by run.sh
ROUNDTRIP_ROUTES=(
  "10.11.8|jellyfin-10.11.8|10.11.8|jellyfin-12.4-from-10|/data"
  "12.4-from-123|jellyfin-12.4-from-123|12.2.0|jellyfin-12.4-from-123|/config"
)
