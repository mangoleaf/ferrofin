#!/usr/bin/env bash
# The round trip's routes, sourced by run.sh: route|fixture directory|the generation Ferrofin adopts it
# as (the boot log's, as in gates/adoption/run.sh's FIXTURE_TABLE). Each fixture is one
# gates/adoption/build-fixtures.sh builds. A route is named for its source, and every route returns to
# the same Jellyfin release (rt_jellyfin_image). More routes come with the export (its history rule).
# shellcheck disable=SC2034  # read by run.sh
ROUNDTRIP_ROUTES=(
  "10.11.8|jellyfin-10.11.8|10.11.8"
  "12.4-from-123|jellyfin-12.4-from-123|12.2.0"
)
