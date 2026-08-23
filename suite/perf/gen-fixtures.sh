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
ARTISTS=${FIXTURE_ARTISTS:-0}
LIVETV=${FIXTURE_LIVETV:-0}
GUIDE_DAYS=${FIXTURE_GUIDE_DAYS:-400}   # the XMLTV window; "now" must stay inside it
ALBUMS=${FIXTURE_ALBUMS_PER_ARTIST:-1}
TRACKS=${FIXTURE_TRACKS_PER_ALBUM:-3}
ROOT=fixtures/media

command -v ffmpeg >/dev/null || { echo "ffmpeg required on host to generate fixtures"; exit 1; }

rm -rf "$ROOT"
mkdir -p "$ROOT/movies" "$ROOT/tv" fixtures/.src
MASTER=fixtures/.src/master.mkv
POSTER=fixtures/.src/poster.jpg
TRACK=fixtures/.src/master.flac

# 1s A/V test clip + a poster frame. Real streams => real probe on both servers.
# The clip also carries an embedded subtitle track and an attached font: the subtitle
# stream/playlist endpoints and /Attachments need them on a real item (the mimetype tag
# is what makes both servers list the attachment). No TTF on the host → no attachment.
SRT=fixtures/.src/sub.srt
printf '1\n00:00:00,000 --> 00:00:01,000\nParity subtitle\n' > "$SRT"
FONT="$(fc-match -f '%{file}' 'DejaVu Sans' 2>/dev/null || true)"
case "$FONT" in *.ttf) ;; *) FONT="" ;; esac   # fc-match always answers; only a TrueType will do
[ -f "${FONT:-}" ] || FONT="$(find /usr/share/fonts -name '*.ttf' 2>/dev/null | head -1 || true)"
ATTACH=()
if [ -f "${FONT:-}" ]; then
  cp "$FONT" fixtures/.src/font.ttf
  ATTACH=(-attach fixtures/.src/font.ttf -metadata:s:t:0 mimetype=application/x-truetype-font)
else
  echo "!! no TTF font on this host: the attachment fixture is skipped" >&2
fi
ffmpeg -y -loglevel error \
  -f lavfi -i testsrc=duration=1:size=320x240:rate=5 \
  -f lavfi -i sine=duration=1:frequency=440 \
  -i "$SRT" "${ATTACH[@]}" \
  -map 0:v -map 1:a -map 2:s -metadata:s:s:0 language=eng \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -c:s srt -shortest "$MASTER"
ffmpeg -y -loglevel error -i "$MASTER" -frames:v 1 "$POSTER"
# A 30 s variant for the FIRST series only: trickplay samples one frame every 10 s, so a
# 1 s clip yields no tiles at all; three 30 s episodes give both servers something to tile.
LONG=fixtures/.src/master-long.mkv
ffmpeg -y -loglevel error \
  -f lavfi -i testsrc=duration=30:size=320x240:rate=5 \
  -f lavfi -i sine=duration=30:frequency=440 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest "$LONG"
# 2s audio-only clip for the music library (tags are stamped per track below).
ffmpeg -y -loglevel error -f lavfi -i sine=duration=2:frequency=330 -c:a flac "$TRACK"

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
  # Movie 0001 carries a real IMDb id (remote fetchers stay off, so nothing is fetched for
  # it) — the opt-in remote-subtitle journey searches OpenSubtitles by that id on both.
  local ids=""
  [ "$i" -eq 1 ] && ids='<uniqueid type="imdb" default="true">tt0111161</uniqueid>'
  cat > "$1/movie.nfo" <<XML
<?xml version="1.0" encoding="utf-8"?>
<movie><title>$2</title><year>2020</year>$ids<genre>$g1</genre><genre>$g2</genre><studio>$st</studio><actor><name>$a1</name><role>Lead</role><type>Actor</type></actor><director>$a2</director></movie>
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
    if [ "$s" -eq 1 ]; then link "$LONG" "$d/Series $sn S01E$en.mkv"
    else link "$MASTER" "$d/Series $sn S01E$en.mkv"; fi
  done
done

# Music: Artist/Album/track.flac with real tags (artist/album/genre/title/track) so both
# servers scan identical artists, albums and music genres — makes the by-name music
# endpoints (Artists/{name}, MusicGenres/{name}, the instant mixes) and the /Audio/*
# stream family testable against a real audio item. Tags differ per file, so these are
# re-muxed copies (`-c copy`, milliseconds each) rather than hardlinks.
MGENRES=(Rock Jazz Ambient)
if [ "$ARTISTS" -gt 0 ]; then
  echo "generating $ARTISTS artists x $ALBUMS albums x $TRACKS tracks..."
  for a in $(seq 1 "$ARTISTS"); do
    an=$(printf '%02d' "$a"); artist="Artist $an"; genre="${MGENRES[$((a % 3))]}"
    for b in $(seq 1 "$ALBUMS"); do
      bn=$(printf '%02d' "$b"); album="Album $bn"; d="$ROOT/music/$artist/$album"
      mkdir -p "$d"
      for t in $(seq 1 "$TRACKS"); do
        tn=$(printf '%02d' "$t")
        # Titles are unique across the whole library: the read diff aligns array items by
        # Name, and a shuffled instant mix with duplicate names would fall back to a
        # positional compare and flag spuriously.
        title="$artist $album Track $tn"
        ffmpeg -y -loglevel error -i "$TRACK" -c copy \
          -metadata artist="$artist" -metadata album_artist="$artist" -metadata album="$album" \
          -metadata title="$title" -metadata track="$t" -metadata genre="$genre" -metadata date=2022 \
          "$d/$tn - $title.flac"
      done
    done
  done
fi

# Live TV: an M3U tuner (two channels, both the endless broadcast the `livetv-source`
# compose sidecar loops from loop.ts — a tuner stream has to be a network source that
# never ends for both servers to treat it as live) and an XMLTV guide with hourly
# programmes over FIXTURE_GUIDE_DAYS, so "what's on now" resolves until the window runs
# out (regenerate the fixture then). Both servers read the M3U/XMLTV from the shared mount.
# The two channels play the same broadcast but must have DISTINCT URLs: Jellyfin derives a
# channel's id from the MD5 of its URL line and would collapse them into one channel.
# loop.ts is 60 s: docker-compose.yml passes that length to the sidecar for pacing.
if [ "$LIVETV" -gt 0 ]; then
  echo "generating live tv fixture..."
  mkdir -p "$ROOT/livetv"
  # a 60 s MPEG-TS loop clip: the "broadcast"
  ffmpeg -y -loglevel error \
    -f lavfi -i testsrc=duration=60:size=320x240:rate=5 \
    -f lavfi -i sine=duration=60:frequency=220 \
    -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest -f mpegts "$ROOT/livetv/loop.ts"
  cat > "$ROOT/livetv/channels.m3u" <<M3U
#EXTM3U
#EXTINF:-1 tvg-id="parity1" tvg-chno="1" tvg-name="Parity One",Parity One
http://livetv-source:8000/live.ts?ch=1
#EXTINF:-1 tvg-id="parity2" tvg-chno="2" tvg-name="Parity Two",Parity Two
http://livetv-source:8000/live.ts?ch=2
M3U
  python3 - "$ROOT/livetv/guide.xml" "$GUIDE_DAYS" <<'PY'
import sys, datetime
out, days = sys.argv[1], int(sys.argv[2])
start = datetime.datetime.now(datetime.timezone.utc).replace(minute=0, second=0, microsecond=0) - datetime.timedelta(days=1)
fmt = lambda t: t.strftime("%Y%m%d%H%M%S +0000")
with open(out, "w") as f:
    f.write('<?xml version="1.0" encoding="UTF-8"?>\n<tv generator-info-name="parity">\n')
    for ch in ("parity1", "parity2"):
        f.write(f'  <channel id="{ch}"><display-name>{ch}</display-name></channel>\n')
    for h in range(24 * days):
        t0 = start + datetime.timedelta(hours=h)
        t1 = t0 + datetime.timedelta(hours=1)
        for ch in ("parity1", "parity2"):
            # titles unique per channel: the read diff aligns list items by Name
            f.write(f'  <programme start="{fmt(t0)}" stop="{fmt(t1)}" channel="{ch}">'
                    f'<title lang="en">Parity Show {h % 24:02d} on {ch}</title>'
                    f'<desc lang="en">Hour {h % 24} on {ch}</desc><category lang="en">News</category></programme>\n')
    f.write('</tv>\n')
PY
fi

echo "done: $((MOVIES + SERIES*EPS)) video items + $((ARTISTS*ALBUMS*TRACKS)) tracks under $ROOT"
