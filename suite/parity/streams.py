#!/usr/bin/env python3
"""Stream-signature differential: the direct-play, HLS, subtitle and trickplay family.

These ops return bytes or playlists, not JSON, so — as assets.py does for images — each
one is reduced to the *properties a player depends on* and those are diffed:

  direct play     sha256 of the body (both servers serve the same hardlinked fixture file),
                  Content-Type, Accept-Ranges, and a Range request's 206 + Content-Range
  HEAD            status class + Content-Type family (headers only)
  playlists       the parsed m3u8: every tag (per-instance noise stripped — hosts, tokens,
                  session/source ids), segment count, EXTINF durations rounded, codecs,
                  bandwidth bucket. The first real check the suite has of HLS semantics.
  segments        what ffprobe says about the bytes: container magic, codecs, resolution,
                  duration rounded (both servers transcode the same 1 s clip)
  subtitle text   the converted subtitle, exactly (whitespace and terminators included)
  trickplay tile  decoded image format + dimensions, the served-file headers
                  (Accept-Ranges, Content-Disposition), and the two JPEG format choices a
                  client can observe: chroma subsampling and optimized-vs-standard Huffman

Each row records HOW it was verified (`verification_method`, see OP_METHOD): "body-diff"
where the bytes/text themselves were compared, "property" where they could not be and named
properties were compared instead. gen-ledger.py keeps property rows OUT of the headline
deep-verified count and renders them in their own section, so "response + read-back diffed
clean" keeps meaning exactly that. Most of this layer is necessarily "property": a HEAD has
no body, a playlist carries per-instance ids, and a transcoded segment or a re-encoded JPEG
is the output of two independent encoders.

Transcodes run for real on both servers (the bench images carry ffmpeg); the clip is one
second long, so a segment costs seconds. Trickplay is generated on the Shows library only
(24 episodes) by enabling the library option and running the task on both servers.

Emits parity/stream-results.json; gen-ledger.py ingests it like the asset layer.

Run via sweep.sh, or directly against provisioned servers:
  FERROFIN_URL=... JELLYFIN_URL=... parity/streams.py
Offline self-check:
  parity/streams.py --check
"""
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sweep import http, get_json, bring_up, ROOT          # noqa: E402
from assets import raw_headers, ct_family, image_info      # noqa: E402

DEVICE_ID = "parity-streams"
DURATION_TOL_S = 0.5          # EXTINF / probed duration rounding (both encode the same clip)
BANDWIDTH_BUCKET = 250_000    # master-playlist BANDWIDTH compared in buckets of this many bps
TRICKPLAY_WAIT_S = 600        # both servers must finish the trickplay task within this
# Two independent JPEG writers differ by a fraction of a percent when they agree on
# subsampling, quantization and entropy coding (measured: 73810 vs 73734 bytes = 1.001x).
# Above this ratio the tile row keeps its green signature but carries a recorded note: a
# real encoder-settings divergence showed up here as 3.44x, and it must not be silent.
TILE_BYTES_RATIO_NOTE = 1.10
# Query noise that legitimately differs per instance/session: stripped from playlist URIs
# and tags before comparing.
NOISE_PARAMS = {"api_key", "apikey", "deviceid", "playsessionid", "mediasourceid",
                "transcodingjobid", "runtimeticks", "actualsegmentlengthticks", "tag"}

# ------------------------------------------------------------- helpers

def sha(b):
    return hashlib.sha256(b).hexdigest()


def file_sig(base, path, token, ranged=False):
    """(2, 'file', sha256, content-type, accept-ranges[, 206, content-range, sha256]) or
    (status_class, ''). assets.file_sig minus Content-Disposition: a stream is not a
    download, neither server sets one here."""
    st, h, body = raw_headers("GET", base, path, token)
    if st != 200:
        return (st // 100, "")
    sig = (2, "file", sha(body), (h.get("content-type") or "").lower(),
           (h.get("accept-ranges") or "").lower())
    if ranged:
        rs, rh, rbody = raw_headers("GET", base, path, token, {"Range": "bytes=0-99"})
        sig += (rs, rh.get("content-range") or "", sha(rbody))
    return sig


def head_sig(base, path, token):
    st, h, _ = raw_headers("HEAD", base, path, token)
    return (st // 100, ct_family(h.get("content-type")) if st == 200 else "")


def strip_noise(uri):
    """A playlist URI with the per-instance query params removed (order-insensitive)."""
    p = urllib.parse.urlsplit(uri)
    keep = sorted((k, v) for k, v in urllib.parse.parse_qsl(p.query, keep_blank_values=True)
                  if k.lower() not in NOISE_PARAMS)
    return urllib.parse.urlunsplit(("", "", p.path, urllib.parse.urlencode(keep), ""))


def normalize_attrs(attrs):
    """An attribute list with noise stripped from URIs and bandwidths bucketed, sorted."""
    out = []
    for m in re.finditer(r'([A-Z0-9-]+)=("[^"]*"|[^,]*)', attrs):
        k, v = m.group(1), m.group(2)
        if k == "URI":
            v = '"' + strip_noise(v.strip('"')) + '"'
        elif k in ("BANDWIDTH", "AVERAGE-BANDWIDTH"):
            try:
                v = str(int(v) // BANDWIDTH_BUCKET)
            except ValueError:
                pass
        out.append(f"{k}={v}")
    return ",".join(sorted(out))


def normalize_playlist(text):
    """The comparable shape of an m3u8: the HEADER tags (everything before the first segment
    or variant) as a set — RFC 8216 gives them no order — and the SEGMENT section in order,
    because a player reads it positionally: EXTINF durations rounded, per-segment tags
    (DISCONTINUITY, KEY, MAP, BYTERANGE, …) kept where they stand, segment URIs replaced by
    their ordinal (they carry per-server ids), a variant URI glued to its STREAM-INF so a
    swapped BANDWIDTH/URI pairing cannot compare equal. Noise params are stripped
    everywhere and BANDWIDTH is bucketed."""
    header, body = [], []
    pending_inf = None
    in_body = False
    segments = 0
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        if line.startswith("#EXTINF:"):
            in_body = True
            dur = line[len("#EXTINF:"):].split(",", 1)[0]
            try:
                body.append(f"#EXTINF:{round(float(dur) / DURATION_TOL_S) * DURATION_TOL_S:g}")
            except ValueError:
                body.append(line)
        elif line.startswith("#EXT-X-STREAM-INF:"):
            in_body = True
            pending_inf = "#EXT-X-STREAM-INF:" + normalize_attrs(line.partition(":")[2])
        elif line.startswith("#EXT-X-MEDIA:") or line.startswith("#EXT-X-MAP:") or line.startswith("#EXT-X-KEY:"):
            tag, _, attrs = line.partition(":")
            (body if in_body else header).append(f"{tag}:{normalize_attrs(attrs)}")
        elif line.startswith("#"):
            (body if in_body else header).append(line)
        elif pending_inf is not None:
            body.append(pending_inf + " → " + strip_noise(line))
            pending_inf = None
        else:
            in_body = True
            segments += 1
            body.append(f"segment#{segments}")
    return tuple(sorted(header)) + tuple(body)


def playlist_sig(base, path, token):
    """(2, 'm3u8', content-type, normalised playlist) and the raw body; (status_class, '')
    on a non-200 (body still returned for the caller's note)."""
    st, h, body = raw_headers("GET", base, path, token)
    if st != 200:
        return (st // 100, ""), body
    ct = (h.get("content-type") or "").lower().split(";")[0].strip()
    return (2, "m3u8", ct, normalize_playlist(body.decode("utf-8", "replace"))), body


def first_segment(playlist_text, playlist_url):
    """The first segment URI of a media playlist, resolved against the playlist's URL."""
    for line in playlist_text.splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            return urllib.parse.urljoin(playlist_url, line)
    return None


def magic(body):
    if body[:1] == b"\x47" and len(body) > 188 and body[188:189] == b"\x47":
        return "mpegts"
    if body[4:8] in (b"ftyp", b"styp", b"moof", b"moov"):
        return "mp4"
    if body[:2] == b"\xff\xf1" or body[:2] == b"\xff\xf9":
        return "adts"
    if body[:3] == b"ID3" or body[:2] in (b"\xff\xfb", b"\xff\xf3", b"\xff\xf2"):
        return "mp3"
    if body[:4] == b"fLaC":
        return "flac"
    if body[:4] == b"\x1a\x45\xdf\xa3":
        return "matroska"
    return "unknown"


def probe(body):
    """ffprobe on the bytes: (sorted 'type:codec[:WxH]' list, duration rounded) or None."""
    with tempfile.NamedTemporaryFile(suffix=".bin", delete=True) as f:
        f.write(body)
        f.flush()
        try:
            out = subprocess.run(
                ["ffprobe", "-v", "error", "-show_entries",
                 "stream=codec_type,codec_name,width,height:format=duration", "-of", "json", f.name],
                capture_output=True, text=True, timeout=60)
        except (OSError, subprocess.TimeoutExpired):
            return None
    if out.returncode != 0:
        return None
    try:
        info = json.loads(out.stdout)
    except ValueError:
        return None
    streams = []
    for s in info.get("streams", []):
        desc = f"{s.get('codec_type')}:{s.get('codec_name')}"
        if s.get("width") and s.get("height"):
            desc += f":{s['width']}x{s['height']}"
        streams.append(desc)
    try:
        dur = round(float(info.get("format", {}).get("duration", 0)) / DURATION_TOL_S) * DURATION_TOL_S
    except (TypeError, ValueError):
        dur = None
    return (tuple(sorted(streams)), dur)


def segment_sig(base, url, token):
    """`url` is absolute (resolved against the playlist URL) and lives under `base`."""
    st, h, body = raw_headers("GET", base, url[len(base):], token)
    if st != 200:
        return (st // 100, "")
    return (2, "segment", (h.get("content-type") or "").lower().split(";")[0], magic(body), probe(body))


def text_sig(base, path, token):
    st, h, body = raw_headers("GET", base, path, token)
    if st != 200:
        return (st // 100, "")
    # Exact text: a faithful writer is byte-identical (BOM, line terminators, blank
    # lines included) — collapsing whitespace would hide a stray CR in a cue.
    text = body.decode("utf-8", "replace")
    return (2, (h.get("content-type") or "").lower().split(";")[0], text)


def jpeg_sampling(body):
    """The (h, v) sampling factors of each component in a JPEG's SOF marker, i.e. its
    chroma-subsampling regime: ((2,2),(1,1),(1,1)) is 4:2:0, ((1,1),(1,1),(1,1)) is 4:4:4.
    None for a non-JPEG or an unparseable one.

    Two independent JPEG writers are never byte-identical, so the bytes cannot be diffed —
    but the subsampling is a discrete choice both must make the same way, and it is the one
    that shows: a 4:4:4 tile is ~3x the bytes of the 4:2:0 one, and Jellyfin derives
    `TrickplayInfo.Bandwidth` (a client's scrub-prefetch budget) straight from the tile's
    file length. Skia's SkJpegEncoder defaults to Downsample::k420 for every JPEG Jellyfin
    writes, so 4:2:0 is the oracle."""
    if not body[:2] == b"\xff\xd8":
        return None
    i = 2
    while i + 4 <= len(body):
        if body[i] != 0xFF:
            return None
        marker, length = body[i + 1], (body[i + 2] << 8) | body[i + 3]
        if 0xC0 <= marker <= 0xC2:          # SOF0 baseline / SOF1 extended / SOF2 progressive
            count = body[i + 9]
            # Component *ids* are deliberately excluded: jpeg-encoder numbers them 0/1/2
            # where libjpeg (Skia) uses the JFIF-conventional 1/2/3, and decoders bind
            # components positionally, so no client can observe that. See LEDGER note.
            return tuple((body[i + 11 + 3 * k] >> 4, body[i + 11 + 3 * k] & 0x0F)
                         for k in range(count))
        i += 2 + length
    return None


def jpeg_huffman(body):
    """"optimized" if the JPEG's Huffman tables were derived from the image, "standard" if
    they are libjpeg's built-in ones, None for a non-JPEG.

    libjpeg's standard AC tables always carry 162 symbols; a table built from the image's
    own statistics carries fewer. Skia sets `optimize_coding`, so Jellyfin's tiles come out
    with (measured) 11/93/11/56 symbols — and on the flat, mostly-DC content of a trickplay
    tile that is a factor of two in bytes for identical pixels and identical quantization
    tables. The exact symbol counts are two optimizers' opinions and are not compared; that
    the encoder optimized at all is a discrete choice both must make the same way."""
    if body[:2] != b"\xff\xd8":
        return None
    counts, i = [], 2
    while i + 4 <= len(body):
        if body[i] != 0xFF:
            return None
        marker, length = body[i + 1], (body[i + 2] << 8) | body[i + 3]
        if marker == 0xDA:                 # start of scan: no tables after this
            break
        if marker == 0xC4:                 # DHT (may pack several tables)
            seg, j = body[i + 4:i + 2 + length], 0
            while j + 17 <= len(seg):
                n = sum(seg[j + 1:j + 17])
                counts.append(n)
                j += 17 + n
        i += 2 + length
    if not counts:
        return None
    return "standard" if any(n == 162 for n in counts) else "optimized"


def image_sig(base, path, token):
    """(2, ct_family, format, w, h, accept-ranges, content-disposition, jpeg-sampling,
    jpeg-huffman) and the body's byte length.

    The byte length is RECORDED, never gated: two independent JPEG writers cannot produce
    the same number of bytes, so it rides in the row's note instead of the signature (see
    run()). Everything else is gated.

    The decoded image, plus the served-file headers `file_sig`/`assets.file_sig` already
    compare, plus the two JPEG format choices a client can observe: chroma subsampling and
    whether the entropy coder optimized its tables. The bytes cannot be diffed (two
    independent encoders), so everything about the response that *can* be compared is."""
    st, h, body = raw_headers("GET", base, path, token)
    if st != 200:
        return (st // 100, ""), 0
    return ((2, ct_family(h.get("content-type")), *image_info(body),
             (h.get("accept-ranges") or "").lower(), h.get("content-disposition") or "",
             jpeg_sampling(body), jpeg_huffman(body)),
            len(body))

# ------------------------------------------------------------- context

def resolve(base, token, user):
    """Per-server context. Media source ids come from PlaybackInfo, exactly as a client
    obtains them (they are not necessarily the item id's spelling)."""
    def first(kinds, fields="Path"):
        b = get_json(base, f"/Items?userId={user}&recursive=true&includeItemTypes={kinds}"
                           f"&limit=1&sortBy=SortName&fields={fields}", token) or {}
        it = b.get("Items") or []
        return it[0] if it else {}

    def source_id(item_id):
        info = get_json(base, f"/Items/{item_id}/PlaybackInfo?userId={user}", token) or {}
        sources = info.get("MediaSources") or []
        return (sources[0].get("Id") or item_id) if sources else item_id

    movie = first("Movie", fields="Path,MediaStreams")
    audio = first("Audio")
    episode = first("Episode")
    # The fixture clip's embedded subtitle track, by the server's own stream index (an
    # uploaded external subtitle can shift it, and the two servers number independently).
    embedded = [s.get("Index") for s in movie.get("MediaStreams") or []
                if s.get("Type") == "Subtitle" and not s.get("IsExternal")]
    return {"user": user, "movie": movie.get("Id", ""), "audio": audio.get("Id", ""),
            "episode": episode.get("Id", ""),
            "sub_index": str(embedded[0]) if embedded else "2",
            "movie_src": source_id(movie["Id"]) if movie.get("Id") else "",
            "audio_src": source_id(audio["Id"]) if audio.get("Id") else "",
            "episode_src": source_id(episode["Id"]) if episode.get("Id") else "",
            "movie_ext": os.path.splitext(movie.get("Path") or "")[1].lstrip(".") or "mkv",
            "audio_ext": os.path.splitext(audio.get("Path") or "")[1].lstrip(".") or "flac"}


def hls_query(source, video=True):
    # One play session per media source: Jellyfin keys the transcode job on the session, so
    # a shared id would hand the audio probe the video job's segments.
    q = {"mediaSourceId": source, "deviceId": DEVICE_ID, "playSessionId": f"parity-{source}",
         "audioCodec": "aac", "audioBitRate": "128000", "segmentContainer": "ts"}
    if video:
        q.update({"videoCodec": "h264", "videoBitRate": "1000000", "maxWidth": "320",
                  "transcodingMaxAudioChannels": "2"})
    return urllib.parse.urlencode(q)


UNRESOLVED = (None, "unresolved")

# Every op this layer reports (also what the self-check validates against the spec).
STREAM_OPS = [
    "GET /Videos/{itemId}/stream", "HEAD /Videos/{itemId}/stream",
    "GET /Videos/{itemId}/stream.{container}", "HEAD /Videos/{itemId}/stream.{container}",
    "GET /Audio/{itemId}/stream", "HEAD /Audio/{itemId}/stream",
    "GET /Audio/{itemId}/stream.{container}", "HEAD /Audio/{itemId}/stream.{container}",
    "GET /Audio/{itemId}/universal", "HEAD /Audio/{itemId}/universal",
    "GET /Videos/{itemId}/master.m3u8", "HEAD /Videos/{itemId}/master.m3u8",
    "GET /Videos/{itemId}/main.m3u8", "GET /Videos/{itemId}/live.m3u8",
    "GET /Videos/{itemId}/hls1/{playlistId}/{segmentId}.{container}",
    "GET /Videos/{itemId}/hls/{playlistId}/stream.m3u8",
    "GET /Videos/{itemId}/hls/{playlistId}/{segmentId}.{segmentContainer}",
    "GET /Audio/{itemId}/master.m3u8", "HEAD /Audio/{itemId}/master.m3u8",
    "GET /Audio/{itemId}/main.m3u8",
    "GET /Audio/{itemId}/hls1/{playlistId}/{segmentId}.{container}",
    "GET /Audio/{itemId}/hls/{segmentId}/stream.aac", "GET /Audio/{itemId}/hls/{segmentId}/stream.mp3",
    "GET /Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/Stream.{routeFormat}",
    "GET /Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/{routeStartPositionTicks}/Stream.{routeFormat}",
    "GET /Videos/{itemId}/{mediaSourceId}/Subtitles/{index}/subtitles.m3u8",
    "GET /Videos/{itemId}/Trickplay/{width}/tiles.m3u8",
    "GET /Videos/{itemId}/Trickplay/{width}/{index}.jpg",
]

# How each op is verified — the ledger's headline count means "the response body (or a
# write's read-back) diffed clean", so a row that compared something else must say so
# rather than ride in on the default.
#
#   "body-diff"  the response body itself was compared: sha256 of the served file (both
#                servers hardlink the SAME fixture file, so the bytes must match exactly),
#                or the exact converted subtitle text.
#   "property"   the body could not be compared, so named properties were: a HEAD has no
#                body at all; a playlist carries per-instance ids/hosts/session tokens; a
#                transcoded segment is two independent ffmpeg runs; a JPEG tile is two
#                independent encoders. Real verification, weaker than a diff.
OP_METHOD = {
    # file_sig — sha256 of the body + Content-Type/Accept-Ranges (+ a Range 206).
    "GET /Videos/{itemId}/stream": "body-diff",
    "GET /Videos/{itemId}/stream.{container}": "body-diff",
    "GET /Audio/{itemId}/stream": "body-diff",
    "GET /Audio/{itemId}/stream.{container}": "body-diff",
    "GET /Audio/{itemId}/universal": "body-diff",
    # text_sig — the exact converted subtitle text, byte for byte.
    "GET /Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/Stream.{routeFormat}": "body-diff",
    "GET /Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/{routeStartPositionTicks}/Stream.{routeFormat}": "body-diff",
    # head_sig — status class + Content-Type family. No body exists to diff.
    "HEAD /Videos/{itemId}/stream": "property",
    "HEAD /Videos/{itemId}/stream.{container}": "property",
    "HEAD /Audio/{itemId}/stream": "property",
    "HEAD /Audio/{itemId}/stream.{container}": "property",
    "HEAD /Audio/{itemId}/universal": "property",
    "HEAD /Videos/{itemId}/master.m3u8": "property",
    "HEAD /Audio/{itemId}/master.m3u8": "property",
    # playlist_sig — the normalised m3u8: tags, segment count, rounded EXTINF, bucketed
    # BANDWIDTH, ordinal segment URIs. The raw text carries per-instance ids and tokens.
    "GET /Videos/{itemId}/master.m3u8": "property",
    "GET /Videos/{itemId}/main.m3u8": "property",
    "GET /Videos/{itemId}/live.m3u8": "property",
    "GET /Videos/{itemId}/hls/{playlistId}/stream.m3u8": "property",
    "GET /Audio/{itemId}/master.m3u8": "property",
    "GET /Audio/{itemId}/main.m3u8": "property",
    "GET /Videos/{itemId}/{mediaSourceId}/Subtitles/{index}/subtitles.m3u8": "property",
    "GET /Videos/{itemId}/Trickplay/{width}/tiles.m3u8": "property",
    # segment_sig — what ffprobe says about two independent transcodes of the same clip.
    "GET /Videos/{itemId}/hls1/{playlistId}/{segmentId}.{container}": "property",
    "GET /Videos/{itemId}/hls/{playlistId}/{segmentId}.{segmentContainer}": "property",
    "GET /Audio/{itemId}/hls1/{playlistId}/{segmentId}.{container}": "property",
    "GET /Audio/{itemId}/hls/{segmentId}/stream.aac": "property",
    "GET /Audio/{itemId}/hls/{segmentId}/stream.mp3": "property",
    # image_sig — decoded format/dimensions, the served-file headers, and the JPEG format
    # choices (subsampling, entropy-coder optimization). Two independent encoders, so the
    # tile's bytes can never be diffed.
    "GET /Videos/{itemId}/Trickplay/{width}/{index}.jpg": "property",
}

# The two ops that depend on trickplay actually having been generated on both servers.
TRICKPLAY_OPS = (
    "GET /Videos/{itemId}/Trickplay/{width}/tiles.m3u8",
    "GET /Videos/{itemId}/Trickplay/{width}/{index}.jpg",
)


def trickplay_width(base, token, c):
    item = get_json(base, f"/Items/{c['episode']}?userId={c['user']}&fields=Trickplay", token) or {}
    tp = item.get("Trickplay") or {}
    for widths in tp.values():
        for w in widths:
            return str(w)
    return None


def enable_trickplay(base, token):
    """Turn trickplay extraction on for the Shows library (LibraryOptions) and start the
    generation task.

    Returns `(task_id, reason)`. `task_id` is None when nothing could be started, and
    `reason` then names the step that failed — every failure here silently degrades both
    trickplay rows to UNRESOLVED, so the cause has to travel with them — a bare
    `unresolved` once hid a real 404 on the enabling write."""
    folders = get_json(base, "/Library/VirtualFolders", token) or []
    shows = next((f for f in folders if (f.get("CollectionType") or "").lower() == "tvshows"), None)
    if not shows:
        return None, "no tvshows library in GET /Library/VirtualFolders"
    opts = shows.get("LibraryOptions") or {}
    opts["EnableTrickplayImageExtraction"] = True
    # Id only, exactly as jellyfin-web does: UpdateLibraryOptionsDto carries a Guid Id and
    # nothing else, so posting a Name here would be testing a route no client can use.
    st, _ = http("POST", f"{base}/Library/VirtualFolders/LibraryOptions", token,
                 json.dumps({"Id": shows.get("ItemId"), "LibraryOptions": opts}))
    if st >= 300:
        return None, f"POST /Library/VirtualFolders/LibraryOptions -> {st}"
    tasks = get_json(base, "/ScheduledTasks", token) or []
    # By key first: both servers also ship a "Move Trickplay Images" task whose name matches.
    task = (next((t for t in tasks if (t.get("Key") or "") == "RefreshTrickplayImages"), None)
            or next((t for t in tasks if "generate trickplay" in (t.get("Name") or "").lower()), None))
    if not task:
        return None, "no RefreshTrickplayImages scheduled task"
    st, _ = http("POST", f"{base}/ScheduledTasks/Running/{task['Id']}", token, "")
    if st >= 300:
        return None, f"POST /ScheduledTasks/Running/{task['Id']} -> {st}"
    return task["Id"], ""


def wait_task(base, token, task_id):
    """True once the task is Idle again (within TRICKPLAY_WAIT_S)."""
    if not task_id:
        return False
    deadline = time.monotonic() + TRICKPLAY_WAIT_S
    time.sleep(2)
    while time.monotonic() < deadline:
        cur = get_json(base, f"/ScheduledTasks/{task_id}", token) or {}
        if cur.get("State") == "Idle":
            return True
        time.sleep(2)
    return False

# ------------------------------------------------------------- probes

def signatures(base, token, c):
    """{op_key: signature} for one server. An op whose fixture item is missing (no movie, no
    track, no episode) is recorded UNRESOLVED — untested — rather than probed with a
    placeholder id, which would make two 404s look like parity.

    Keys starting with `_` are observations, not ops: run() strips them out and folds them
    into a row's recorded note. They never become rows and never gate anything."""
    out = {}
    m, a, e, u = c["movie"], c["audio"], c["episode"], c["user"]
    ms, as_, es = c["movie_src"], c["audio_src"], c["episode_src"]
    mext, aext = c["movie_ext"], c["audio_ext"]
    if not m or not a:
        # Every op below needs the movie and the track; say so once instead of faking it.
        missing = [k for k in STREAM_OPS]
        return {k: UNRESOLVED for k in missing}

    # --- direct play ------------------------------------------------------------------
    out["GET /Videos/{itemId}/stream"] = file_sig(base, f"/Videos/{m}/stream?static=true", token, ranged=True)
    out["HEAD /Videos/{itemId}/stream"] = head_sig(base, f"/Videos/{m}/stream?static=true", token)
    out["GET /Videos/{itemId}/stream.{container}"] = file_sig(base, f"/Videos/{m}/stream.{mext}?static=true", token)
    out["HEAD /Videos/{itemId}/stream.{container}"] = head_sig(base, f"/Videos/{m}/stream.{mext}?static=true", token)
    out["GET /Audio/{itemId}/stream"] = file_sig(base, f"/Audio/{a}/stream?static=true", token, ranged=True)
    out["HEAD /Audio/{itemId}/stream"] = head_sig(base, f"/Audio/{a}/stream?static=true", token)
    out["GET /Audio/{itemId}/stream.{container}"] = file_sig(base, f"/Audio/{a}/stream.{aext}?static=true", token)
    out["HEAD /Audio/{itemId}/stream.{container}"] = head_sig(base, f"/Audio/{a}/stream.{aext}?static=true", token)
    universal = (f"/Audio/{a}/universal?userId={u}&deviceId={DEVICE_ID}&container={aext}"
                 f"&maxStreamingBitrate=140000000&audioCodec=flac,aac,mp3")
    out["GET /Audio/{itemId}/universal"] = file_sig(base, universal, token)
    out["HEAD /Audio/{itemId}/universal"] = head_sig(base, universal, token)

    # --- HLS: video --------------------------------------------------------------------
    vq = hls_query(ms)
    out["GET /Videos/{itemId}/master.m3u8"], _ = playlist_sig(base, f"/Videos/{m}/master.m3u8?{vq}", token)
    out["HEAD /Videos/{itemId}/master.m3u8"] = head_sig(base, f"/Videos/{m}/master.m3u8?{vq}", token)
    main_url = f"/Videos/{m}/main.m3u8?{vq}"
    out["GET /Videos/{itemId}/main.m3u8"], main_body = playlist_sig(base, main_url, token)
    seg = first_segment(main_body.decode("utf-8", "replace"), base + main_url) if main_body else None
    out["GET /Videos/{itemId}/hls1/{playlistId}/{segmentId}.{container}"] = (
        segment_sig(base, seg, token) if seg and "/hls1/" in seg else UNRESOLVED)
    live_url = f"/Videos/{m}/live.m3u8?{vq}"
    out["GET /Videos/{itemId}/live.m3u8"], _ = playlist_sig(base, live_url, token)
    # The legacy `hls/{playlistId}/…` routes serve files from the transcode directory by
    # name; no 10.11 playlist hands a client such a name (live.m3u8 lists ffmpeg's own
    # `{playlistId}{index}.ts` files, which do not resolve through these routes on either
    # server), so the contract left to check is "unknown playlist → 404" on both.
    st, _, _ = raw_headers("GET", base, f"/Videos/{m}/hls/parity-missing/stream.m3u8", token)
    out["GET /Videos/{itemId}/hls/{playlistId}/stream.m3u8"] = (st // 100, "")
    st, _, _ = raw_headers("GET", base, f"/Videos/{m}/hls/parity-missing/0.ts", token)
    out["GET /Videos/{itemId}/hls/{playlistId}/{segmentId}.{segmentContainer}"] = (st // 100, "")

    # --- HLS: audio --------------------------------------------------------------------
    aq = hls_query(as_, video=False)
    out["GET /Audio/{itemId}/master.m3u8"], _ = playlist_sig(base, f"/Audio/{a}/master.m3u8?{aq}", token)
    out["HEAD /Audio/{itemId}/master.m3u8"] = head_sig(base, f"/Audio/{a}/master.m3u8?{aq}", token)
    amain_url = f"/Audio/{a}/main.m3u8?{aq}"
    out["GET /Audio/{itemId}/main.m3u8"], amain_body = playlist_sig(base, amain_url, token)
    aseg = first_segment(amain_body.decode("utf-8", "replace"), base + amain_url) if amain_body else None
    out["GET /Audio/{itemId}/hls1/{playlistId}/{segmentId}.{container}"] = (
        segment_sig(base, aseg, token) if aseg and "/hls1/" in aseg else UNRESOLVED)
    # The legacy audio segment routes read a transcode-directory file by name; nothing in
    # 10.11 hands a client such a name, so the contract left is "unknown segment → 404".
    st, _, _ = raw_headers("GET", base, f"/Audio/{a}/hls/parity-missing/stream.aac", token)
    out["GET /Audio/{itemId}/hls/{segmentId}/stream.aac"] = (st // 100, "")
    st, _, _ = raw_headers("GET", base, f"/Audio/{a}/hls/parity-missing/stream.mp3", token)
    out["GET /Audio/{itemId}/hls/{segmentId}/stream.mp3"] = (st // 100, "")

    # --- subtitles (the fixture's embedded eng track) ---------------------------------
    si = c["sub_index"]
    sub_sigs = tuple(text_sig(base, f"/Videos/{m}/{ms}/Subtitles/{si}/Stream.{fmt}", token)
                     for fmt in ("vtt", "srt", "json"))
    out["GET /Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/Stream.{routeFormat}"] = sub_sigs
    subs_url = f"/Videos/{m}/{ms}/Subtitles/{si}/subtitles.m3u8?segmentLength=10"
    out["GET /Videos/{itemId}/{mediaSourceId}/Subtitles/{index}/subtitles.m3u8"], subs_body = playlist_sig(
        base, subs_url, token)
    # The segment the subtitle playlist names IS the {routeStartPositionTicks} route, with
    # the query a player sends (AddVttTimeMap=true, CopyTimestamps=true …): fetch that one.
    seg = first_segment(subs_body.decode("utf-8", "replace"), base + subs_url) if subs_body else None
    out["GET /Videos/{routeItemId}/{routeMediaSourceId}/Subtitles/{routeIndex}/{routeStartPositionTicks}/Stream.{routeFormat}"] = (
        text_sig(base, seg[len(base):], token) if seg else UNRESOLVED)

    # --- trickplay (enabled + generated on the Shows library by run()) ----------------
    width = trickplay_width(base, token, c)
    if width:
        tiles_url = f"/Videos/{e}/Trickplay/{width}/tiles.m3u8?mediaSourceId={es}"
        out["GET /Videos/{itemId}/Trickplay/{width}/tiles.m3u8"], tiles_body = playlist_sig(
            base, tiles_url, token)
        # The tile a client fetches is whatever the playlist names (its first segment).
        tile = first_segment(tiles_body.decode("utf-8", "replace"), base + tiles_url) if tiles_body else None
        out["GET /Videos/{itemId}/Trickplay/{width}/{index}.jpg"], out["_tile_bytes"] = (
            image_sig(base, tile[len(base):], token) if tile else (UNRESOLVED, 0))
    else:
        out["GET /Videos/{itemId}/Trickplay/{width}/tiles.m3u8"] = UNRESOLVED
        out["GET /Videos/{itemId}/Trickplay/{width}/{index}.jpg"] = UNRESOLVED
    return out


def describe(sig):
    """A short human note for a signature (playlists and probes are long)."""
    s = json.dumps(sig, default=str)
    return s if len(s) <= 400 else s[:400] + "…"

# ------------------------------------------------------------- run

def run(ferrofin_url, jellyfin_url):
    ht, hu = bring_up(ferrofin_url, "ferrofin")
    jt, ju = bring_up(jellyfin_url, "jellyfin")
    hc, jc = resolve(ferrofin_url, ht, hu), resolve(jellyfin_url, jt, ju)
    if not shutil.which("ffprobe"):
        print("!! ffprobe not on PATH: segment rows compare MIME + container magic only",
              file=sys.stderr)
    # Both generation tasks run concurrently; wait for each.
    (th, why_h), (tj, why_j) = enable_trickplay(ferrofin_url, ht), enable_trickplay(jellyfin_url, jt)
    tp_h, tp_j = wait_task(ferrofin_url, ht, th), wait_task(jellyfin_url, jt, tj)
    if th and not tp_h:
        why_h = f"generation task {th} still running after {TRICKPLAY_WAIT_S}s"
    if tj and not tp_j:
        why_j = f"generation task {tj} still running after {TRICKPLAY_WAIT_S}s"
    blocked = "; ".join(f"{n} could not generate trickplay ({w})"
                        for n, w in (("ferrofin", why_h), ("jellyfin", why_j)) if w)
    if blocked:
        print(f"!! {blocked}", file=sys.stderr)
    print(f"trickplay generated: ferrofin={tp_h} jellyfin={tp_j}")
    hs, js = signatures(ferrofin_url, ht, hc), signatures(jellyfin_url, jt, jc)
    obs_h = {k: hs.pop(k) for k in [k for k in hs if k.startswith("_")]}
    obs_j = {k: js.pop(k) for k in [k for k in js if k.startswith("_")]}
    rows = {}
    for op in sorted(hs):
        h, j = hs[op], js.get(op)
        if h == UNRESOLVED and j == UNRESOLVED:
            # Neither server handed out a usable reference (e.g. a legacy playlist id): not
            # evidence of parity — recorded as untested, never as verified.
            rows[op] = {"deep_verified": None, "verification_method": OP_METHOD[op],
                        "classification": "",
                        "note": "unresolved on both servers: no reference to probe with"}
            continue
        ok = j is not None and h == j
        rows[op] = {"deep_verified": bool(ok),
                    "verification_method": OP_METHOD[op],
                    "classification": "" if ok else "flagged: stream signature diff vs Jellyfin (verify)",
                    "note": f"H={describe(h)} J={describe(j)}"}
    # A trickplay row that could not be probed must say *why* it could not be probed;
    # otherwise a broken enabling write reads as an inconclusive fixture problem.
    if blocked:
        for op in TRICKPLAY_OPS:
            if op in rows and not rows[op]["deep_verified"]:
                rows[op]["note"] = f'{rows[op]["note"]} [{blocked}]'

    # The one thing about the tile that cannot be gated: its byte length. Two independent
    # JPEG writers never agree exactly, and Jellyfin derives `TrickplayInfo.Bandwidth`
    # (the scrub-prefetch budget a client is handed) straight from it —
    # `ceil(bytes * 8 / TileWidth / TileHeight / (Interval / 1000))`. So the difference is
    # measured and RECORDED on the row every run rather than dropped: a green signature
    # with a 3x byte gap is exactly what this layer used to look like, and the record is
    # what makes a regression legible. Only the ratio is judged, and only for the note.
    tile = "GET /Videos/{itemId}/Trickplay/{width}/{index}.jpg"
    if tile in rows:
        record_tile_bytes(rows[tile], obs_h.get("_tile_bytes", 0), obs_j.get("_tile_bytes", 0))
    return rows


def record_tile_bytes(row, bh, bj):
    """Fold the two tile byte lengths into `row`'s note, and — when they are far enough
    apart to mean a settings divergence rather than encoder noise — into its recorded
    classification. Mutates `row`; returns nothing."""
    if not (bh and bj):
        return
    ratio = max(bh, bj) / min(bh, bj)
    row["note"] += f" [tile bytes H={bh} J={bj} ratio={ratio:.3f}]"
    if row["deep_verified"] and ratio > TILE_BYTES_RATIO_NOTE:
        row["classification"] = (
            f"accepted-with-note: format choices match (4:2:0, optimized Huffman, same "
            f"geometry and headers) but the tile is {ratio:.2f}x Jellyfin's byte length "
            f"({bh} vs {bj}), which scales TrickplayInfo.Bandwidth by the same factor — "
            f"re-check the encoder settings in ferrofin-drawing::write_jpeg")


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    ferrofin = os.environ.get("FERROFIN_URL", "http://localhost:18096")
    jellyfin = os.environ.get("JELLYFIN_URL", "http://localhost:18097")
    rows = run(ferrofin, jellyfin)
    out = {"generated_by": "suite/parity/streams.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "rows": rows}
    with open(os.path.join(ROOT, "suite/parity/stream-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"])
    print(f"wrote parity/stream-results.json — {len(rows)} stream ops, {ok} deep-verified")


def selfcheck():
    import glob
    import inspect
    spec = json.load(open(sorted(glob.glob(os.path.join(ROOT, "contracts/jellyfin-openapi-*.json")))[-1]))
    valid = {f"{m.upper()} {p}" for p, item in spec["paths"].items() for m in item if m in ("get", "head")}
    declared = set(re.findall(r'"((?:GET|HEAD) /[^"]+)"', inspect.getsource(signatures)))
    assert declared == set(STREAM_OPS), (declared ^ set(STREAM_OPS))
    assert set(TRICKPLAY_OPS) <= set(STREAM_OPS), set(TRICKPLAY_OPS) - set(STREAM_OPS)
    bad = sorted(k for k in declared if k not in valid)
    assert not bad, f"stream op-keys not in spec: {bad}"
    # playlist normalisation: noise stripped, durations rounded, segments counted.
    a = normalize_playlist("#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXTINF:2.98,\nhls1/main/0.ts?api_key=A&runtimeTicks=1\n")
    b = normalize_playlist("#EXT-X-TARGETDURATION:3\n#EXTM3U\n#EXTINF:3.02,\nhls1/x/0.ts?api_key=B&runtimeTicks=2\n")
    assert a == b, (a, b)   # header order is a set; durations round; segment ids are ordinals
    m1 = normalize_playlist('#EXT-X-STREAM-INF:BANDWIDTH=1200000,CODECS="avc1.64001e,mp4a.40.2"\nmain.m3u8?DeviceId=1&videoCodec=h264')
    m2 = normalize_playlist('#EXT-X-STREAM-INF:CODECS="avc1.64001e,mp4a.40.2",BANDWIDTH=1100000\nmain.m3u8?videoCodec=h264&DeviceId=2')
    assert m1 == m2, (m1, m2)
    # the segment section is positional: a moved DISCONTINUITY is a difference
    d1 = normalize_playlist("#EXTM3U\n#EXTINF:1,\na.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:1,\nb.ts\n")
    d2 = normalize_playlist("#EXTM3U\n#EXT-X-DISCONTINUITY\n#EXTINF:1,\na.ts\n#EXTINF:1,\nb.ts\n")
    assert d1 != d2
    assert magic(b"\x47" + b"\x00" * 187 + b"\x47") == "mpegts"
    assert magic(b"\x00\x00\x00\x18ftypisom") == "mp4"
    assert strip_noise("a/b.ts?PlaySessionId=x&foo=1") == "a/b.ts?foo=1"
    # Every op must declare how it is verified — a row that silently defaulted to
    # "body-diff" would be counted in the ledger's headline as a diffed body.
    assert set(OP_METHOD) == set(STREAM_OPS), set(OP_METHOD) ^ set(STREAM_OPS)
    assert set(OP_METHOD.values()) <= {"body-diff", "property"}, set(OP_METHOD.values())
    # Only the sha256/exact-text signatures may claim a body diff. Everything a HEAD, a
    # playlist, a transcoded segment or a re-encoded image produces is a property.
    src = inspect.getsource(signatures)
    for op, method in OP_METHOD.items():
        line = next(ln for ln in src.splitlines() if f'"{op}"' in ln)
        diffed = "file_sig(" in line or "text_sig(" in line or op.endswith("Stream.{routeFormat}")
        assert (method == "body-diff") == diffed, (op, method, line.strip())
    # jpeg_sampling reads the SOF marker: 4:2:0 (Skia's default) vs 4:4:4.
    def sof(luma_h, luma_v):
        return (b"\xff\xd8"
                + b"\xff\xe0\x00\x04\x00\x00"                    # a short APP0 to skip
                + b"\xff\xc0\x00\x11\x08\x00\x10\x00\x10\x03"  # SOF0, 16x16, 3 components
                + bytes([1, luma_h << 4 | luma_v, 0, 2, 0x11, 1, 3, 0x11, 1]))
    assert jpeg_sampling(sof(2, 2)) == ((2, 2), (1, 1), (1, 1)), jpeg_sampling(sof(2, 2))
    assert jpeg_sampling(sof(1, 1)) == ((1, 1), (1, 1), (1, 1))
    assert jpeg_sampling(sof(2, 2)) != jpeg_sampling(sof(1, 1))
    assert jpeg_sampling(b"\x89PNG\r\n\x1a\n") is None

    # jpeg_huffman: 162 AC symbols is libjpeg's standard table; fewer means optimized.
    def dht(symbols):
        counts = [0] * 16
        counts[3] = symbols                      # all symbols at code length 4
        body = bytes([0x10]) + bytes(counts) + bytes(symbols)
        return (b"\xff\xd8"
                + b"\xff\xc4" + (len(body) + 2).to_bytes(2, "big") + body
                + b"\xff\xda\x00\x02")
    assert jpeg_huffman(dht(162)) == "standard"
    assert jpeg_huffman(dht(93)) == "optimized"
    assert jpeg_huffman(b"\x89PNG\r\n\x1a\n") is None
    assert jpeg_huffman(b"\xff\xd8\xff\xda\x00\x02") is None   # no DHT at all
    # Observations (keys starting with "_") are stripped before rows are built, so they
    # can never become a row or be looked up in OP_METHOD.
    assert all(not k.startswith("_") for k in STREAM_OPS)
    assert TILE_BYTES_RATIO_NOTE > 1.0
    # The tile byte lengths are always recorded, and a settings-level gap (the 3.44x this
    # layer actually measured before ferrofin-drawing matched Skia's encoder) becomes a
    # classification the ledger prints beside the green row.
    near = {"deep_verified": True, "classification": "", "note": "sig"}
    record_tile_bytes(near, 73810, 73734)
    assert "ratio=1.001" in near["note"] and near["classification"] == "", near
    far = {"deep_verified": True, "classification": "", "note": "sig"}
    record_tile_bytes(far, 253353, 73734)
    assert "ratio=3.436" in far["note"], far
    assert far["classification"].startswith("accepted-with-note:"), far
    assert "Bandwidth" in far["classification"]
    none_yet = {"deep_verified": None, "classification": "", "note": "sig"}
    record_tile_bytes(none_yet, 0, 73734)      # nothing measured → nothing recorded
    assert none_yet["note"] == "sig" and none_yet["classification"] == ""
    print(f"ok: {len(declared)} stream op-keys valid + verification methods, "
          f"playlist normalisation, jpeg sampling, magic, noise strip")


if __name__ == "__main__":
    main()
