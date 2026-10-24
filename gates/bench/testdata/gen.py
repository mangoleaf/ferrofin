#!/usr/bin/env python3
"""Generate the benchmark test-data media tree (deterministic, seeded).

    gen.py OUT_DIR [--seed N] [--scale F]

Writes OUT_DIR/{movies,shows,music} plus OUT_DIR/.templates. Every media file is a
copy_file_range clone of one of five ffmpeg-generated template clips — separate inodes
(so per-file mtimes can differ), sharing extents on btrfs/xfs (zero extra disk) and a
real copy elsewhere (~20 GB on ext4) — with Kodi/Jellyfin NFO sidecars and locally drawn
images so Jellyfin 10.11.8 scans it with every remote provider off.

Host requirements: ffmpeg/ffprobe (libx264, libx265) and Pillow (`python3 -c "import PIL"`)
for the image pool. Pillow's default bitmap font makes image bytes host-dependent, which
only changes blurhash values, never structure.

Cases (see PLAN_BENCHMARK_V3 §3):
  C1  multi-version movies      "Title (Y) - 1080p.mkv" + "Title (Y) - 2160p.mkv"
  C2  HDR10 4K 5.1 movies       hevc_2160p_hdr10_51 template
  C3  multi-track movies        2 audio languages + 2 SRT + chapters
  C4  the streaming movie       "Bench Stream (2024)", 3 min at 8 Mbps
"""

import argparse
import concurrent.futures as cf
import datetime as dt
import json
import os
import random
import shutil
import subprocess
import sys
from pathlib import Path
from xml.sax.saxutils import escape

# ── scale-1 counts (D5) ──────────────────────────────────────────────────────
MOVIES = 3000
SERIES = 250
ARTISTS = 300
ALBUMS = 800
TRACKS_PER_ALBUM = 10
PEOPLE = 5000
GENRES = 30
STUDIOS = 200
MULTI_VERSION = 40  # C1
HDR = 50  # C2
MULTI_TRACK = 300  # C3
IMAGE_POOL = 512  # distinct posters/fanart drawn; items share from the pool

ADJ = """Silent Broken Golden Hidden Last First Lost Red Black White Blue Green Dark Bright
Cold Burning Frozen Wild Quiet Endless Distant Secret Iron Glass Paper Stone Velvet Hollow
Crimson Silver Empty Final Second Northern Southern Eastern Western Ancient Modern Electric
Wooden Crooked Straight Narrow Wide Little Great Bitter Sweet Sour Falling Rising Sleeping
Waking Running Walking Drowning Flying Burning Fading Shining Rusted Painted Sacred Cursed""".split()
NOUN = """River Mountain City Harbor Garden Window Door Road Bridge Tower Forest Ocean Island
Desert Valley Storm Winter Summer Autumn Spring Night Morning Shadow Light Fire Water Earth
Wind Stone Glass Mirror Clock Letter Song Dance Dream Memory Promise Secret Lie Truth Game
Machine Engine Signal Circle Square Line Edge Center Kingdom Empire Republic Colony Village
Station Market Theater Library Museum Cathedral Palace Prison Hospital School Factory Farm""".split()
FIRST = """James Mary Robert Patricia John Jennifer Michael Linda David Elizabeth William
Barbara Richard Susan Joseph Jessica Thomas Sarah Charles Karen Christopher Lisa Daniel
Nancy Matthew Betty Anthony Sandra Mark Ashley Donald Kimberly Steven Emily Paul Donna
Andrew Michelle Joshua Carol Kenneth Amanda Kevin Melissa Brian Deborah George Stephanie
Timothy Rebecca Ronald Sharon Edward Laura Jason Cynthia Jeffrey Kathleen Ryan Amy Jacob
Angela Gary Shirley Nicholas Anna Eric Brenda Jonathan Pamela Stephen Emma Larry Nicole
Justin Helen Scott Samantha Brandon Katherine Benjamin Christine Samuel Debra Gregory""".split()
LAST = """Smith Johnson Williams Brown Jones Garcia Miller Davis Rodriguez Martinez Hernandez
Lopez Gonzalez Wilson Anderson Thomas Taylor Moore Jackson Martin Lee Perez Thompson White
Harris Sanchez Clark Ramirez Lewis Robinson Walker Young Allen King Wright Scott Torres
Nguyen Hill Flores Green Adams Nelson Baker Hall Rivera Campbell Mitchell Carter Roberts
Gomez Phillips Evans Turner Diaz Parker Cruz Edwards Collins Reyes Stewart Morris Morales
Murphy Cook Rogers Gutierrez Ortiz Morgan Cooper Peterson Bailey Reed Kelly Howard Ramos
Kim Cox Ward Richardson Watson Brooks Chavez Wood James Bennett Gray Mendoza Ruiz Hughes
Price Alvarez Castillo Sanders Patel Myers Long Ross Foster Jimenez Powell Jenkins Perry
Russell Sullivan Bell Coleman Butler Henderson Barnes Gonzales Fisher Vasquez Simmons""".split()
GENRE_NAMES = """Action Adventure Animation Biography Comedy Crime Documentary Drama Family
Fantasy History Horror Music Musical Mystery Romance Science-Fiction Sport Thriller War
Western Noir Superhero Martial-Arts Disaster Heist Road Coming-of-Age Political Satire""".split()
MPAA = ["G", "PG", "PG-13", "R", "NC-17"]

AUDIO_TEMPLATE = "aac.m4a"


def sh(cmd, **kw):
    r = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if r.returncode != 0:
        sys.exit(f"{cmd[0]} failed ({r.returncode}): {' '.join(cmd)}\n{r.stderr[-2000:]}")


def clone(src: Path, dst: Path):
    """Zero-copy clone where the fs supports it (btrfs/xfs reflink), plain copy otherwise.
    Separate inodes either way, so per-file mtimes can differ."""
    dst.parent.mkdir(parents=True, exist_ok=True)
    try:
        with open(src, "rb") as fi, open(dst, "wb") as fo:
            size = os.fstat(fi.fileno()).st_size
            off = 0
            while off < size:
                n = os.copy_file_range(fi.fileno(), fo.fileno(), size - off, off, off)
                if n == 0:
                    raise OSError("copy_file_range returned 0")
                off += n
    except OSError:
        shutil.copyfile(src, dst)


def nfo(tag, fields, children=""):
    body = "".join(f"  <{k}>{escape(str(v))}</{k}>\n" for k, v in fields)
    return f'<?xml version="1.0" encoding="utf-8"?>\n<{tag}>\n{body}{children}</{tag}>\n'


def person_xml(name, role=None, ptype="Actor", order=0):
    r = f"    <role>{escape(role)}</role>\n" if role else ""
    return f"  <actor>\n    <name>{escape(name)}</name>\n{r}    <type>{ptype}</type>\n    <order>{order}</order>\n  </actor>\n"


# ── templates ────────────────────────────────────────────────────────────────
def make_templates(tdir: Path):
    done = tdir / "done"  # written after every clip verified, so a killed run redoes them all
    if done.exists():
        return
    tdir.mkdir(parents=True, exist_ok=True)
    srt_en = tdir / "en.srt"
    srt_fr = tdir / "fr.srt"
    for p, words in ((srt_en, ("Hello", "World")), (srt_fr, ("Bonjour", "Monde"))):
        p.write_text("1\n00:00:00,500 --> 00:00:02,000\n%s\n\n2\n00:00:02,500 --> 00:00:04,500\n%s\n" % words)
    chap = tdir / "chapters.txt"
    chap.write_text(";FFMETADATA1\n" + "".join(
        f"[CHAPTER]\nTIMEBASE=1/1000\nSTART={i * 30000}\nEND={(i + 1) * 30000}\ntitle=Chapter {i + 1}\n" for i in range(6)))
    v = ["-f", "lavfi", "-i", "testsrc2=s=1920x1080:r=24"]
    a = ["-f", "lavfi", "-i", "sine=f=440:r=48000"]
    a2 = ["-f", "lavfi", "-i", "sine=f=660:r=48000"]
    x264 = ["-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p"]
    jobs = {
        "h264_1080p_aac.mkv": ["ffmpeg", "-y", *v, *a, "-t", "5", *x264, "-b:v", "2M", "-c:a", "aac", "-b:a", "128k"],
        "hevc_2160p_hdr10_51.mkv": [
            "ffmpeg", "-y", "-f", "lavfi", "-i", "testsrc2=s=3840x2160:r=24", *a, "-t", "5",
            "-c:v", "libx265", "-preset", "ultrafast", "-pix_fmt", "yuv420p10le",
            "-color_primaries", "bt2020", "-color_trc", "smpte2084", "-colorspace", "bt2020nc",
            "-x265-params", "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:"
            "master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400:hdr10=1",
            "-b:v", "8M", "-c:a", "eac3", "-ac", "6", "-b:a", "640k"],
        "h264_multitrack.mkv": [
            "ffmpeg", "-y", *v, *a, *a2, "-i", str(srt_en), "-i", str(srt_fr), "-i", str(chap),
            "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:s", "-map", "4:s", "-map_metadata", "5",
            "-t", "5", *x264, "-b:v", "2M", "-c:a", "aac", "-b:a", "128k", "-c:s", "srt",
            "-metadata:s:a:0", "language=eng", "-metadata:s:a:1", "language=fra",
            "-metadata:s:s:0", "language=eng", "-metadata:s:s:1", "language=fra"],
        "h264_8mbps_180s.mkv": [
            "ffmpeg", "-y", *v, *a, "-i", str(srt_en), "-i", str(srt_fr), "-i", str(chap),
            "-map", "0:v", "-map", "1:a", "-map", "2:s", "-map", "3:s", "-map_metadata", "4",
            "-t", "180", *x264, "-b:v", "8M", "-minrate", "8M", "-maxrate", "8M", "-bufsize", "16M",
            "-c:a", "aac", "-b:a", "192k", "-c:s", "srt",
            "-metadata:s:s:0", "language=eng", "-metadata:s:s:1", "language=fra"],
        AUDIO_TEMPLATE: ["ffmpeg", "-y", *a, "-t", "5", "-c:a", "aac", "-b:a", "160k"],
    }
    with cf.ThreadPoolExecutor() as ex:
        list(ex.map(lambda kv: sh(kv[1] + [str(tdir / kv[0])]), list(jobs.items())))
    for k in jobs:
        sh(["ffprobe", str(tdir / k)])
    done.touch()


def make_image_pool(tdir: Path, rng: random.Random):
    """Distinct locally drawn images so blurhashes vary; items clone from the pool."""
    from PIL import Image, ImageDraw
    pool = tdir / "images"
    if (pool / "done").exists():
        return pool
    pool.mkdir(exist_ok=True)
    kinds = {"poster": (400, 600, "jpg"), "fanart": (1280, 720, "jpg"), "logo": (400, 155, "png"),
             "landscape": (800, 450, "jpg"), "album": (500, 500, "jpg")}
    for kind, (w, h, ext) in kinds.items():
        for i in range(IMAGE_POOL):
            mode = "RGBA" if ext == "png" else "RGB"
            base = tuple(rng.randrange(256) for _ in range(3))
            im = Image.new(mode, (w, h), base + ((0,) if mode == "RGBA" else ()))
            d = ImageDraw.Draw(im)
            for _ in range(rng.randrange(3, 9)):
                x0, y0 = rng.randrange(w), rng.randrange(h)
                col = tuple(rng.randrange(256) for _ in range(3)) + ((255,) if mode == "RGBA" else ())
                d.ellipse([x0, y0, x0 + rng.randrange(w // 4, w), y0 + rng.randrange(h // 4, h)], fill=col)
            d.text((10, 10), f"{kind} {i}", fill=(255, 255, 255) + ((255,) if mode == "RGBA" else ()))
            im.save(pool / f"{kind}{i}.{ext}", quality=80)
    (pool / "done").touch()
    return pool


# ── vocab ────────────────────────────────────────────────────────────────────
class Vocab:
    def __init__(self, rng):
        self.rng = rng
        self.titles = set()
        self.people = rng.sample([f"{f} {l}" for f in FIRST for l in LAST], PEOPLE)
        self.genres = GENRE_NAMES[:GENRES]
        combos = [f"{a} {n} {s}" for a in ADJ for n in NOUN for s in ("Pictures", "Films", "Studios", "Entertainment")]
        self.studios = rng.sample(combos, STUDIOS)

    def title(self):
        rng = self.rng
        while True:
            form = rng.randrange(4)
            t = [f"{rng.choice(ADJ)} {rng.choice(NOUN)}", f"The {rng.choice(ADJ)} {rng.choice(NOUN)}",
                 f"{rng.choice(NOUN)} of {rng.choice(NOUN)}", f"{rng.choice(NOUN)} {rng.choice(NOUN)}"][form]
            key = t[4:] if t.startswith("The ") else t  # unique on the sort name, not just the title
            if key not in self.titles:
                self.titles.add(key)
                return t

    def date(self, lo=1950, hi=2025):
        rng = self.rng
        return dt.date(rng.randrange(lo, hi + 1), rng.randrange(1, 13), rng.randrange(1, 29))

    def added(self):
        # DateCreated spread over the last 3 years so "Latest" rows have an order.
        return (dt.datetime(2026, 9, 1) - dt.timedelta(seconds=self.rng.randrange(3 * 365 * 86400))).strftime("%Y-%m-%d %H:%M:%S")

    def cast(self, n):
        rng = self.rng
        names = rng.sample(self.people, n + 2)
        xml = "".join(person_xml(nm, role=f"{rng.choice(NOUN)} {i}", order=i) for i, nm in enumerate(names[:n]))
        xml += person_xml(names[n], ptype="Director") + person_xml(names[n + 1], ptype="Writer")
        return xml, names[n], names[n + 1]


def common_fields(v: Vocab, title, year, premiered, rng):
    return [("title", title), ("sorttitle", title[4:] if title.startswith("The ") else title),
            ("year", year), ("premiered", premiered), ("rating", round(rng.uniform(3, 9.5), 1)),
            ("mpaa", rng.choice(MPAA)), ("plot", f"{title}: a story about {rng.choice(NOUN).lower()} and {rng.choice(NOUN).lower()}."),
            ("dateadded", v.added())] + [("genre", g) for g in rng.sample(v.genres, rng.randrange(2, 5))] + \
           [("studio", s) for s in rng.sample(v.studios, rng.randrange(1, 3))]


# ── movies ───────────────────────────────────────────────────────────────────
def gen_movies(out: Path, t: Path, pool: Path, v: Vocab, rng: random.Random, n: int, cases):
    root = out / "movies"
    # one disjoint draw, sliced, so summary.json's case counts are what was produced
    picks = rng.sample(range(n), min(n, cases["hdr"] + cases["multi_track"] + cases["multi_version"]))
    hdr_ids = set(picks[:cases["hdr"]])
    mt_ids = set(picks[cases["hdr"]:cases["hdr"] + cases["multi_track"]])
    mv_ids = set(picks[cases["hdr"] + cases["multi_track"]:])
    cases.update(hdr=len(hdr_ids), multi_track=len(mt_ids), multi_version=len(mv_ids))  # produced, not requested
    for i in range(n):
        title, year = v.title(), rng.randrange(1950, 2026)
        d = root / f"{title} ({year})"
        d.mkdir(parents=True)
        tmpl = "hevc_2160p_hdr10_51.mkv" if i in hdr_ids else "h264_multitrack.mkv" if i in mt_ids else "h264_1080p_aac.mkv"
        if i in mv_ids:
            clone(t / "h264_1080p_aac.mkv", d / f"{title} ({year}) - 1080p.mkv")
            clone(t / "hevc_2160p_hdr10_51.mkv", d / f"{title} ({year}) - 2160p.mkv")
        else:
            clone(t / tmpl, d / f"{title} ({year}).mkv")
        cast, _, _ = v.cast(rng.randrange(4, 9))
        fields = common_fields(v, title, year, v.date(year, year), rng) + [
            ("tagline", f"{rng.choice(ADJ)} {rng.choice(NOUN).lower()}."), ("runtime", rng.randrange(80, 160)),
            ("tag", rng.choice(NOUN).lower())]
        ids = f'  <uniqueid type="tmdb" default="true">{100000 + i}</uniqueid>\n  <uniqueid type="imdb">tt{1000000 + i:07d}</uniqueid>\n'
        (d / "movie.nfo").write_text(nfo("movie", fields, ids + cast))
        clone(pool / f"poster{rng.randrange(IMAGE_POOL)}.jpg", d / "poster.jpg")
        clone(pool / f"fanart{rng.randrange(IMAGE_POOL)}.jpg", d / "fanart.jpg")
        if rng.random() < 0.20:
            clone(pool / f"logo{rng.randrange(IMAGE_POOL)}.png", d / "logo.png")
        if rng.random() < 0.10:
            clone(pool / f"landscape{rng.randrange(IMAGE_POOL)}.jpg", d / "landscape.jpg")
    # C4 — the streaming movie, fixed name so ids.json / ttfs.py can find it.
    d = root / "Bench Stream (2024)"
    d.mkdir()
    clone(t / "h264_8mbps_180s.mkv", d / "Bench Stream (2024).mkv")
    cast, _, _ = v.cast(5)
    (d / "movie.nfo").write_text(nfo("movie", common_fields(v, "Bench Stream", 2024, "2024-06-01", rng) + [("runtime", 3)],
                                     '  <uniqueid type="tmdb" default="true">999999</uniqueid>\n' + cast))
    clone(pool / "poster0.jpg", d / "poster.jpg")
    clone(pool / "fanart0.jpg", d / "fanart.jpg")
    return n + 1


# ── shows ────────────────────────────────────────────────────────────────────
def gen_shows(out: Path, t: Path, pool: Path, v: Vocab, rng: random.Random, n: int):
    root = out / "shows"
    episodes = 0
    for i in range(n):
        title, year = v.title(), rng.randrange(1960, 2025)
        d = root / f"{title} ({year})"
        d.mkdir(parents=True)
        cast, _, _ = v.cast(rng.randrange(4, 9))
        ids = f'  <uniqueid type="tvdb" default="true">{200000 + i}</uniqueid>\n  <uniqueid type="tmdb">{300000 + i}</uniqueid>\n'
        (d / "tvshow.nfo").write_text(nfo("tvshow", common_fields(v, title, year, v.date(year, year), rng) +
                                          [("status", rng.choice(["Continuing", "Ended"]))], ids + cast))
        clone(pool / f"poster{rng.randrange(IMAGE_POOL)}.jpg", d / "poster.jpg")
        clone(pool / f"fanart{rng.randrange(IMAGE_POOL)}.jpg", d / "fanart.jpg")
        for s in range(1, rng.randrange(1, 5) + 1):
            sd = d / f"Season {s:02d}"
            sd.mkdir()
            (sd / "season.nfo").write_text(nfo("season", [("title", f"Season {s}"), ("seasonnumber", s), ("dateadded", v.added())]))
            clone(pool / f"poster{rng.randrange(IMAGE_POOL)}.jpg", sd / "folder.jpg")
            for e in range(1, rng.randrange(6, 19) + 1):
                base = f"{title} ({year}) - S{s:02d}E{e:02d} - {rng.choice(ADJ)} {rng.choice(NOUN)}"
                clone(t / "h264_1080p_aac.mkv", sd / f"{base}.mkv")
                aired = dt.date(min(year + s - 1, 2025), 1, 1) + dt.timedelta(days=7 * e)
                guests = "".join(person_xml(nm, role="Guest", ptype="GuestStar", order=k) for k, nm in enumerate(rng.sample(v.people, 2)))
                (sd / f"{base}.nfo").write_text(nfo("episodedetails", [
                    ("title", base.split(" - ")[-1]), ("season", s), ("episode", e), ("aired", aired.isoformat()),
                    ("plot", f"Episode {e} of season {s}."), ("rating", round(rng.uniform(5, 9.5), 1)),
                    ("dateadded", v.added())], guests))
                episodes += 1
    return n, episodes


# ── music ────────────────────────────────────────────────────────────────────
def gen_music(out: Path, t: Path, pool: Path, v: Vocab, rng: random.Random, artists: int, albums: int):
    root = out / "music"
    names = [f"{rng.choice(ADJ)} {rng.choice(NOUN)}s" if rng.random() < 0.5 else rng.choice(v.people) for _ in range(artists)]
    names = list(dict.fromkeys(names))
    jobs = []
    for i in range(albums):
        artist = names[i % len(names)]
        album = v.title()
        year = rng.randrange(1960, 2026)
        genre = rng.choice(["Rock", "Pop", "Jazz", "Electronic", "Classical", "Hip-Hop", "Folk", "Metal"])
        ad = root / artist / f"{album} ({year})"
        ad.mkdir(parents=True, exist_ok=True)
        art = root / artist / "artist.nfo"
        if not art.exists():
            art.write_text(nfo("artist", [("title", artist), ("biography", f"{artist} formed in {year - 3}.")]))
        (ad / "album.nfo").write_text(nfo("album", [("title", album), ("artist", artist), ("year", year), ("genre", genre),
                                                     ("dateadded", v.added())]))
        clone(pool / f"album{rng.randrange(IMAGE_POOL)}.jpg", ad / "folder.jpg")
        for k in range(1, TRACKS_PER_ALBUM + 1):
            tt = f"{rng.choice(ADJ)} {rng.choice(NOUN)}"
            dst = ad / f"{k:02d} - {tt}.m4a"
            jobs.append(["ffmpeg", "-y", "-i", str(t / AUDIO_TEMPLATE), "-c", "copy",
                         "-metadata", f"title={tt}", "-metadata", f"artist={artist}", "-metadata", f"album_artist={artist}",
                         "-metadata", f"album={album}", "-metadata", f"track={k}/{TRACKS_PER_ALBUM}", "-metadata", "disc=1/1",
                         "-metadata", f"date={year}", "-metadata", f"genre={genre}", str(dst)])
    with cf.ThreadPoolExecutor(max_workers=os.cpu_count()) as ex:
        list(ex.map(sh, jobs))
    return len(names), albums, len(jobs)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("out")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--scale", type=float, default=1.0, help="multiply every count (0.02 for a smoke run)")
    a = ap.parse_args()
    out = Path(a.out).resolve()
    for bad in ("/mnt/mangonas", "/mnt/nvme0/k3s"):
        if str(out).startswith(bad):
            sys.exit(f"refusing to write under {bad}")
    if (out / "movies").exists() or (out / "summary.json").exists():
        sys.exit(f"{out} already holds a tree — remove it first (never regenerate in place)")
    if a.scale > 2.5:
        sys.exit("--scale above 2.5 exhausts the unique-title space (~14k names)")
    rng = random.Random(a.seed)
    s = lambda n: max(1, round(n * a.scale))
    t = out / ".templates"
    print("templates …", flush=True)
    make_templates(t)
    pool = make_image_pool(t, random.Random(a.seed + 1))
    v = Vocab(rng)
    cases = {"hdr": s(HDR), "multi_track": s(MULTI_TRACK), "multi_version": s(MULTI_VERSION)}
    print("movies …", flush=True)
    movies = gen_movies(out, t, pool, v, rng, s(MOVIES), cases)
    print("shows …", flush=True)
    series, episodes = gen_shows(out, t, pool, v, rng, s(SERIES))
    print("music …", flush=True)
    artists, albums, tracks = gen_music(out, t, pool, v, rng, s(ARTISTS), s(ALBUMS))
    summary = {"seed": a.seed, "scale": a.scale, "movies": movies, "series": series, "episodes": episodes,
               "artists": artists, "albums": albums, "tracks": tracks, **{f"case_{k}": n for k, n in cases.items()},
               "case_stream": "movies/Bench Stream (2024)"}
    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(summary)


if __name__ == "__main__":
    main()
