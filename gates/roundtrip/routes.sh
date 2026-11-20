#!/usr/bin/env bash
# The round trip's routes, sourced by run.sh: route|fixture directory|the generation Ferrofin adopts it
# as (the boot log's, as in gates/adoption/run.sh's FIXTURE_TABLE)|the build log, as <it>.server.log under
# the fixtures, of that Jellyfin release's own first start on the route's source: the code migrations it
# ran are the ones Jellyfin must run on the export. It is the log of the fixture of that release built from
# the source where there is one, else the first start build-fixtures.sh gives the source
# (oracle/first-start-12.4/<fixture>; gates/adoption/synthetic.py FIRST_STARTS)|the data directory
# Ferrofin runs at, and the export reads it at. Each fixture is one gates/adoption/build-fixtures.sh builds.
# A route is named for its source, and every route returns to the same Jellyfin release
# (rt_jellyfin_image), always at /config: a route whose Ferrofin runs at /data (its own image's directory)
# is one whose stored paths the export must rewrite.
# shellcheck disable=SC2034  # read by run.sh
ROUNDTRIP_ROUTES=(
  "10.11.8|jellyfin-10.11.8|10.11.8|jellyfin-12.4-from-10|/data"
  "10.11.9|jellyfin-10.11.9|10.11.8|oracle/first-start-12.4/jellyfin-10.11.9|/data"
  "10.11.10|jellyfin-10.11.10|10.11.11|oracle/first-start-12.4/jellyfin-10.11.10|/data"
  "10.11.11|jellyfin-10.11.11|10.11.11|oracle/first-start-12.4/jellyfin-10.11.11|/data"
  "12.0|jellyfin-12.0|12.0.0|oracle/first-start-12.4/jellyfin-12.0|/data"
  "12.1-from-10|jellyfin-12.1-from-10|12.1.0|oracle/first-start-12.4/jellyfin-12.1-from-10|/data"
  "12.1-from-12|jellyfin-12.1-from-12|12.1.0|oracle/first-start-12.4/jellyfin-12.1-from-12|/data"
  "12.2-from-10|jellyfin-12.2-from-10|12.2.0|oracle/first-start-12.4/jellyfin-12.2-from-10|/data"
  "12.2-from-12|jellyfin-12.2-from-12|12.2.0|oracle/first-start-12.4/jellyfin-12.2-from-12|/data"
  "12.2-from-121|jellyfin-12.2-from-121|12.2.0|oracle/first-start-12.4/jellyfin-12.2-from-121|/data"
  "12.3-from-122|jellyfin-12.3-from-122|12.2.0|jellyfin-12.4-from-123|/data"
  "12.3-from-10|jellyfin-12.3-from-10|12.2.0|oracle/first-start-12.4/jellyfin-12.3-from-10|/data"
  "12.4-from-123|jellyfin-12.4-from-123|12.2.0|jellyfin-12.4-from-123|/config"
)
