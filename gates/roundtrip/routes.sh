#!/usr/bin/env bash
# The round trip's routes, sourced by run.sh. A route is one record, everything that makes it differ from the
# others in its own fields:
#   1. its name, the fixture's without "jellyfin-"
#   2. the fixture directory: one gates/adoption/build-fixtures.sh builds
#   3. the generation Ferrofin adopts it as (the boot log's, as in gates/adoption/run.sh's FIXTURE_TABLE)
#   4. the build log, as <it>.server.log under the fixtures, of that Jellyfin release's own first start on the
#      route's source: the code migrations it ran, less those the source's history records and those the export
#      records as applied (checks_boot.py), are the ones Jellyfin must run on the export (none on the native
#      route). It is the log of the fixture of that release built from the source where there is one, the first
#      start build-fixtures.sh gives the source
#      (oracle/first-start-12.4/<fixture>; gates/adoption/synthetic.py FIRST_STARTS), or, for a source that
#      release wrote itself, that source's own build log. A messy or scale copy has its source's.
#   5. the data directory Ferrofin runs at, and the export reads it at
#   6. its configuration directory, when it is not the data directory's config/ ("-"): a fixture Jellyfin
#      wrote in another layout (synthetic-ready.json's "layout") has its config/ moved there, as a package
#      migration does (gates/adoption/lib.sh ADOPTION_MOVED_*)
#   7. its kind, as the adoption gate's report names it: path (an upgrade path or a fresh install, on the
#      fixtures' media/), messy (messy.py's copies, on media-messy/), scale (scale.py's copy, on
#      media-scale/: adopted, exported and started on with no scan or usage stage, stage_ferrofin.sh says why;
#      its time and size are recorded, not gated) or native (a database Ferrofin created itself, on media/: no
#      fixture and no generation ("-"), its source built by Ferrofin's first start (stage_ferrofin.sh), and the
#      first start of the fresh install whose history its export records, on which Jellyfin runs nothing; its
#      route also runs the version-drift guard, stage_drift.sh)
# A route is named for its source, and every route returns to the same Jellyfin release (rt_jellyfin_image),
# always at /config: a route whose Ferrofin runs elsewhere is one whose stored paths the export must rewrite.
# shellcheck disable=SC2034  # read by run.sh
ROUNDTRIP_ROUTES=(
  "10.11.8|jellyfin-10.11.8|10.11.8|jellyfin-12.4-from-10|/data|-|path"
  "10.11.9|jellyfin-10.11.9|10.11.8|oracle/first-start-12.4/jellyfin-10.11.9|/data|-|path"
  "10.11.10|jellyfin-10.11.10|10.11.11|oracle/first-start-12.4/jellyfin-10.11.10|/data|-|path"
  "10.11.11|jellyfin-10.11.11|10.11.11|oracle/first-start-12.4/jellyfin-10.11.11|/data|-|path"
  "12.0|jellyfin-12.0|12.0.0|oracle/first-start-12.4/jellyfin-12.0|/data|-|path"
  "12.1-from-10|jellyfin-12.1-from-10|12.1.0|oracle/first-start-12.4/jellyfin-12.1-from-10|/data|-|path"
  "12.1-from-12|jellyfin-12.1-from-12|12.1.0|oracle/first-start-12.4/jellyfin-12.1-from-12|/data|-|path"
  "12.2-from-10|jellyfin-12.2-from-10|12.2.0|oracle/first-start-12.4/jellyfin-12.2-from-10|/data|-|path"
  "12.2-from-12|jellyfin-12.2-from-12|12.2.0|oracle/first-start-12.4/jellyfin-12.2-from-12|/data|-|path"
  "12.2-from-121|jellyfin-12.2-from-121|12.2.0|oracle/first-start-12.4/jellyfin-12.2-from-121|/data|-|path"
  "12.2-fresh|jellyfin-12.2-fresh|12.2.0|oracle/first-start-12.4/jellyfin-12.2-fresh|/data|-|path"
  "12.3-from-122|jellyfin-12.3-from-122|12.2.0|jellyfin-12.4-from-123|/data|-|path"
  "12.3-from-10|jellyfin-12.3-from-10|12.2.0|oracle/first-start-12.4/jellyfin-12.3-from-10|/data|-|path"
  "12.3-fresh|jellyfin-12.3-fresh|12.2.0|oracle/first-start-12.4/jellyfin-12.3-fresh|/data|-|path"
  "12.4-from-123|jellyfin-12.4-from-123|12.2.0|jellyfin-12.4-from-123|/config|-|path"
  "12.4-from-10|jellyfin-12.4-from-10|12.2.0|jellyfin-12.4-from-10|/data|-|path"
  "12.4-fresh|jellyfin-12.4-fresh|12.2.0|jellyfin-12.4-fresh|/data|-|path"
  "12.4-debian|jellyfin-12.4-debian|12.2.0|jellyfin-12.4-debian|/var/lib/ferrofin|/etc/ferrofin|path"
  "native|-|-|jellyfin-12.4-fresh|/data|-|native"
  "10.11.11-messy|jellyfin-10.11.11-messy|10.11.11|oracle/first-start-12.4/jellyfin-10.11.11|/data|-|messy"
  "12.4-messy|jellyfin-12.4-messy|12.2.0|jellyfin-12.4-from-123|/data|-|messy"
  "10.11.11-scale|jellyfin-10.11.11-scale|10.11.11|oracle/first-start-12.4/jellyfin-10.11.11|/data|-|scale"
)
