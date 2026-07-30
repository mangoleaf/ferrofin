#!/usr/bin/env bash
# Generate an identical media library both servers scan into the same item count.
# One real (tiny) clip is hardlinked to every path so ffprobe behaves identically on both;
# a real poster.jpg per movie exercises the image-serve/resize path.
set -euo pipefail
cd "$(dirname "$0")"
set -a; [ -f .env ] && . ./.env; set +a

MOVIES=${FIXTURE_MOVIES:-500}
SERIES=${FIXTURE_SERIES:-50}
EPS=${FIXTURE_EPISODES_PER_SERIES:-10}
ROOT=fixtures/media

command -v ffmpeg >/dev/null || { echo "ffmpeg required on host to generate fixtures"; exit 1; }

rm -rf "$ROOT"
mkdir -p "$ROOT/movies" "$ROOT/tv" fixtures/.src
MASTER=fixtures/.src/master.mkv
POSTER=fixtures/.src/poster.jpg

# 1s A/V test clip + a poster frame. Real streams => real probe on both servers.
ffmpeg -y -loglevel error \
  -f lavfi -i testsrc=duration=1:size=320x240:rate=5 \
  -f lavfi -i sine=duration=1:frequency=440 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest "$MASTER"
ffmpeg -y -loglevel error -i "$MASTER" -frames:v 1 "$POSTER"

# cp -l = hardlink (cheap, same fs); fall back to a real copy across filesystems.
link() { cp -l "$1" "$2" 2>/dev/null || cp "$1" "$2"; }

echo "generating $MOVIES movies..."
for i in $(seq 1 "$MOVIES"); do
  n=$(printf '%04d' "$i"); d="$ROOT/movies/Movie $n (2020)"
  mkdir -p "$d"
  link "$MASTER" "$d/Movie $n (2020).mkv"
  link "$POSTER" "$d/poster.jpg"
done

echo "generating $SERIES series x $EPS episodes..."
for s in $(seq 1 "$SERIES"); do
  sn=$(printf '%02d' "$s"); d="$ROOT/tv/Series $sn/Season 01"
  mkdir -p "$d"
  for e in $(seq 1 "$EPS"); do
    en=$(printf '%02d' "$e")
    link "$MASTER" "$d/Series $sn S01E$en.mkv"
  done
done

echo "done: $((MOVIES + SERIES*EPS)) media items under $ROOT"
