#!/usr/bin/env bash
# Build the benchmark test data (PLAN_BENCHMARK_V3 §3):
#   gen.py media tree → Jellyfin 10.11.8 scans it and is seeded over its API → drained
#   → stopped. The resulting DIR/{config,media,ids.json} is what run.sh boots copies of.
#
#   build.sh [DIR] [--scale F] [--seed N]      DIR defaults to $FERROFIN_GATES_DIR/fixtures/bench-testdata
#   build.sh [DIR] --export-pools            read pools from a disposable source copy
#   build.sh [DIR] --prepare-jellyfin          upgrade a copy, scan, verify, stop
#
# Refuses to overwrite: remove DIR/media and DIR/config yourself to rebuild.
set -euo pipefail
CALLER=$PWD
cd "$(dirname "$0")/../../.."
# shellcheck source=gates/lib.sh
source gates/lib.sh
DIR=""; SCALE=1.0; SEED=1; PREPARE=false; EXPORT_POOLS=false
while [ $# -gt 0 ]; do case "$1" in
  --export-pools) EXPORT_POOLS=true; shift;;
  --prepare-jellyfin) PREPARE=true; shift;;
  --scale) SCALE=$2; shift 2;; --seed) SEED=$2; shift 2;; -*) echo "unknown $1" >&2; exit 2;; *) DIR=$1; shift;;
esac; done
[ -n "$DIR" ] || DIR=$(gates_dir build fixtures/bench-testdata) || exit 2
case "$DIR" in /*) ;; *) DIR=$CALLER/$DIR;; esac   # a user-supplied relative DIR is relative to the caller's shell
DIR=$(realpath -m "$DIR")
case "$DIR" in /mnt/mangonas*|/mnt/nvme0/k3s*) echo "refusing to build under $DIR" >&2; exit 1;; esac
# Explicit preparation is separate from fixture generation and timed runs.
if "$PREPARE" && "$EXPORT_POOLS"; then echo "choose preparation or pool export, not both" >&2; exit 2; fi
if "$PREPARE" || "$EXPORT_POOLS"; then
  for t in docker python3 jq sha256sum; do command -v "$t" >/dev/null || { echo "$t not installed" >&2; exit 1; }; done
  for built in config/data/jellyfin.db ids.json media/summary.json; do
    [ -f "$DIR/$built" ] || { echo "build the 10.11.8 source fixture first" >&2; exit 1; }
  done
  PREPARED=$DIR/jellyfin
  if "$EXPORT_POOLS"; then PREPARED=$DIR/cache/pool-export-$$; fi
  [ ! -e "$PREPARED" ] || { echo "$PREPARED exists; choose a new fixture directory or move the old preparation aside" >&2; exit 1; }
  IMAGE=$(cat gates/bench/testdata/jellyfin-image.txt)
  if "$EXPORT_POOLS"; then IMAGE=jellyfin/jellyfin:10.11.8; fi
  IMAGE_ID=$(docker image inspect -f '{{.Id}}' "$IMAGE")
  SOURCE_SHA=$(sha256sum "$DIR/config/data/jellyfin.db" | cut -d' ' -f1)
  IDS_SHA=$(sha256sum "$DIR/ids.json" | cut -d' ' -f1)
  NAME=bench-prepare-$$; PORT=18097
  if "$EXPORT_POOLS"; then NAME=bench-export-pools-$$; fi
  mkdir -p "$PREPARED/cache"
  cp -r --reflink=auto "$DIR/config" "$PREPARED/config"
  docker run -d --name "$NAME" --user "$(id -u):$(id -g)" --memory 8g --memory-swap 8g \
    -p "127.0.0.1:$PORT:8096" -e JELLYFIN_LOG_DIR=/cache/log \
    -v "$PREPARED/config:/config" -v "$PREPARED/cache:/cache" -v "$DIR/media:/media:ro" "$IMAGE" >/dev/null
  trap 'docker logs "$NAME" > "$PREPARED/server.log" 2>&1 || true; docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT
  if "$EXPORT_POOLS"; then
    python3 gates/bench/testdata/seed.py --export-pools "http://127.0.0.1:$PORT" "$DIR/ids.json"
    echo "Pools exported; prepare Jellyfin again because the ID-file hash changed."
    exit 0
  fi
  python3 gates/bench/testdata/seed.py --prepare-jellyfin "http://127.0.0.1:$PORT" \
    "$DIR/ids.json" "$DIR/media/summary.json" "$PREPARED/validation.json"
  docker stop -t 60 "$NAME" >/dev/null
  [ "$(docker inspect -f '{{.State.ExitCode}}' "$NAME")" = 0 ] || {
    echo "Jellyfin preparation did not stop cleanly; no ready marker written" >&2; exit 1;
  }
  jq -n --arg image "$IMAGE" --arg image_id "$IMAGE_ID" --arg source "$SOURCE_SHA" --arg ids "$IDS_SHA" \
    --argjson elapsed "$SECONDS" --slurpfile validation "$PREPARED/validation.json" \
    '{image:$image,image_id:$image_id,source_db_sha256:$source,ids_sha256:$ids,elapsed_s:$elapsed,validation:$validation[0]}' \
    > "$PREPARED/preparation.json"
  echo "Jellyfin preparation ready: $PREPARED ($SECONDS seconds)"
  exit 0
fi

IMAGE=jellyfin/jellyfin:10.11.8; NAME=bench-build; PORT=18096

[ -d "$DIR/config" ] && { echo "$DIR/config exists — remove it (and media/ for a regenerate) to rebuild" >&2; exit 1; }
for t in docker python3 ffmpeg ffprobe; do command -v $t >/dev/null || { echo "$t not installed" >&2; exit 1; }; done
python3 -c "import PIL" 2>/dev/null || { echo "Pillow (python3 PIL) is required for the image pool" >&2; exit 1; }
# summary.json is written last by gen.py, so a killed generation is regenerated, not reused.
[ -f "$DIR/media/summary.json" ] || python3 gates/bench/testdata/gen.py "$DIR/media" --scale "$SCALE" --seed "$SEED"
# config/ + media/ + ids.json are the test data. cache/ holds Jellyfin's scratch AND its
# log (JELLYFIN_LOG_DIR) so nothing has to be deleted afterwards — it is simply not copied.
mkdir -p "$DIR/config" "$DIR/cache"
docker rm -f $NAME >/dev/null 2>&1 || true
docker run -d --name $NAME --user "$(id -u):$(id -g)" -p 127.0.0.1:$PORT:8096 -e JELLYFIN_LOG_DIR=/cache/log \
  -v "$DIR/config:/config" -v "$DIR/cache:/cache" -v "$DIR/media:/media:ro" $IMAGE >/dev/null
trap 'docker rm -f $NAME >/dev/null 2>&1 || true' EXIT
python3 gates/bench/testdata/seed.py "http://127.0.0.1:$PORT" "$DIR/ids.json"
docker stop -t 60 $NAME >/dev/null
docker rm $NAME >/dev/null
LOG=$(cat "$DIR"/cache/log/*.log)
echo "jellyfin log: $(grep -c '\[ERR\]' <<<"$LOG" || true) ERR, $(grep -c '\[WRN\]' <<<"$LOG" || true) WRN, $(grep -ci 'themoviedb\|thetvdb\|musicbrainz\|audiodb\|fanart' <<<"$LOG" || true) remote-provider mentions (expected 0 fetches; plugin registration lines are fine)"
du -sh "$DIR/config" "$DIR/media"
echo "test data ready: $DIR"
