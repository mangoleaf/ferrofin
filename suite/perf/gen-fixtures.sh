#!/usr/bin/env bash
# Generate an identical media library both servers scan into the same item count.
# One real (tiny) clip is hardlinked to every path so ffprobe behaves identically on both;
# a real poster.jpg per movie exercises the image-serve/resize path.
set -euo pipefail
cd "$(dirname "$0")"
set -a; ENVF="${PARITY_ENV:-.env}"; [ -f "$ENVF" ] && . "./$ENVF"; set +a

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

# Deterministic NFO metadata so both servers scan identical genres/studios/people/year — makes the
# by-name endpoints (Genres/Studios/Persons), Years, and similarity/search testable, and the diff
# stays clean because both read the same files. Sets rotate to give real variety without randomness.
GENRES=(Action Drama Comedy Thriller SciFi)
STUDIOS=("Parity Pictures" "Ferrofin Studios")
ACTORS=("Alice Parity" "Bob Parity" "Carol Ferrofin")
movie_nfo() { # $1=dir $2=title $3=index
  local i=$3 g1="${GENRES[$((i % 5))]}" g2="${GENRES[$(((i+2) % 5))]}" st="${STUDIOS[$((i % 2))]}"
  local a1="${ACTORS[$((i % 3))]}" a2="${ACTORS[$(((i+1) % 3))]}"
  cat > "$1/movie.nfo" <<XML
<?xml version="1.0" encoding="utf-8"?>
<movie><title>$2</title><year>2020</year><genre>$g1</genre><genre>$g2</genre><studio>$st</studio><actor><name>$a1</name><role>Lead</role><type>Actor</type></actor><director>$a2</director></movie>
XML
}

echo "generating $MOVIES movies..."
for i in $(seq 1 "$MOVIES"); do
  n=$(printf '%04d' "$i"); d="$ROOT/movies/Movie $n (2020)"
  mkdir -p "$d"
  link "$MASTER" "$d/Movie $n (2020).mkv"
  link "$POSTER" "$d/poster.jpg"
  movie_nfo "$d" "Movie $n" "$i"
done

echo "generating $SERIES series x $EPS episodes..."
for s in $(seq 1 "$SERIES"); do
  sn=$(printf '%02d' "$s"); base="$ROOT/tv/Series $sn"; d="$base/Season 01"
  mkdir -p "$d"
  g1="${GENRES[$((s % 5))]}"
  cat > "$base/tvshow.nfo" <<XML
<?xml version="1.0" encoding="utf-8"?>
<tvshow><title>Series $sn</title><year>2021</year><genre>$g1</genre><studio>${STUDIOS[$((s % 2))]}</studio></tvshow>
XML
  for e in $(seq 1 "$EPS"); do
    en=$(printf '%02d' "$e")
    link "$MASTER" "$d/Series $sn S01E$en.mkv"
  done
done

echo "done: $((MOVIES + SERIES*EPS)) media items under $ROOT"
