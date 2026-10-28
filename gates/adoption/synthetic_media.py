"""Small, invented adoption media. Encoding uses the Jellyfin image's FFmpeg."""

from pathlib import Path
import os
import shutil
import subprocess
from xml.sax.saxutils import escape
import zipfile

from PIL import Image


# Credited on every movie and episode NFO, so Peoples and PeopleBaseItemMap hold rows.
ACTORS = (("Synthetic Actor One", "Lead"), ("Synthetic Actor Two", "Support"))
# The x265 HDR10 parameters of gates/bench/testdata/gen.py's hevc_2160p_hdr10_51 template.
HDR10 = ["-c:v", "libx265", "-preset", "ultrafast", "-pix_fmt", "yuv420p10le",
         "-color_primaries", "bt2020", "-color_trc", "smpte2084", "-colorspace", "bt2020nc",
         "-x265-params", "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:"
         "master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):"
         "max-cll=1000,400:hdr10=1:log-level=error"]


def nfo(path, kind, fields, actors=()):
    path.write_text("<" + kind + ">\n" + "".join(
        f"  <{key}>{escape(str(value))}</{key}>\n" for key, value in fields)
        + "".join(f"  <actor>\n    <name>{escape(name)}</name>\n    <role>{escape(role)}</role>\n"
                  f"    <type>Actor</type>\n    <order>{order}</order>\n  </actor>\n"
                  for order, (name, role) in enumerate(actors))
        + f"</{kind}>\n")


def epub(path, title):
    """A minimal EPUB 3 book: what Jellyfin's book library and its OPF reader need."""
    container = ('<?xml version="1.0"?>\n<container version="1.0" '
                 'xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles>'
                 '<rootfile full-path="book.opf" media-type="application/oebps-package+xml"/>'
                 '</rootfiles></container>\n')
    opf = ('<?xml version="1.0"?>\n<package version="3.0" xmlns="http://www.idpf.org/2007/opf" '
           'unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/">'
           f'<dc:identifier id="id">urn:uuid:00000000-0000-4000-8000-000000000628</dc:identifier>'
           f'<dc:title>{escape(title)}</dc:title><dc:creator>Synthetic Author</dc:creator>'
           '<dc:language>en</dc:language><dc:description>An invented book.</dc:description></metadata>'
           '<manifest><item id="text" href="text.xhtml" media-type="application/xhtml+xml"/></manifest>'
           '<spine><itemref idref="text"/></spine></package>\n')
    text = ('<?xml version="1.0"?>\n<html xmlns="http://www.w3.org/1999/xhtml"><head><title>'
            f'{escape(title)}</title></head><body><p>Synthetic text.</p></body></html>\n')
    with zipfile.ZipFile(path, "w") as book:
        book.writestr("mimetype", "application/epub+zip", zipfile.ZIP_STORED)
        for name, body in (("META-INF/container.xml", container), ("book.opf", opf), ("text.xhtml", text)):
            book.writestr(name, body, zipfile.ZIP_DEFLATED)


def generate(root, image):
    """Create only generated pictures, silent video, tones, and invented tags."""
    root = Path(root).resolve()
    root.mkdir(parents=True, exist_ok=False)

    def encode(args):
        subprocess.run([
            "docker", "run", "--rm", "--network", "none", "--user",
            f"{os.getuid()}:{os.getgid()}",
            "--entrypoint", "/usr/lib/jellyfin-ffmpeg/ffmpeg", "-v", f"{root}:/media",
            image, "-hide_banner", "-loglevel", "error", "-nostdin", *args,
        ], check=True)

    # Chapters every two minutes, the shape of gates/bench/testdata/gen.py's chapters.txt, so every
    # video has Chapters rows (and chapter images once extraction runs).
    (root / "chapters.txt").write_text(";FFMETADATA1\n" + "".join(
        f"[CHAPTER]\nTIMEBASE=1/1000\nSTART={i * 120000}\nEND={(i + 1) * 120000}\ntitle=Chapter {i + 1}\n"
        for i in range(5)))
    video = ["-f", "lavfi", "-i", "color=c=blue:s=320x180:r=1"]
    silence = ["-f", "lavfi", "-i", "anullsrc=r=8000:cl=mono"]
    chapters = ["-f", "ffmetadata", "-i", "/media/chapters.txt"]
    encode([*video, *silence, *chapters, "-map", "0:v", "-map", "1:a", "-map_chapters", "2",
            "-t", "600", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "35", "-c:a", "aac", "-b:a", "8k",
            "-metadata:s:a:0", "language=eng", "/media/template.mkv"])
    # HDR10 video (bench's hevc_2160p_hdr10_51 parameters, at the template's size).
    encode([*video, *silence, *chapters, "-map", "0:v", "-map", "1:a", "-map_chapters", "2",
            "-t", "600", *HDR10, "-crf", "35", "-c:a", "aac", "-b:a", "8k",
            "-metadata:s:a:0", "language=eng", "/media/hdr10.mkv"])
    # Two audio languages (bench's h264_multitrack) and an attached font, so
    # AttachmentStreamInfos holds a row. The font is invented bytes: FFprobe reports an
    # attachment by its MIME type and file name, not its contents.
    (root / "Synthetic Sans.ttf").write_bytes(b"\x00\x01\x00\x00synthetic adoption font\x00")
    encode([*video, *silence, "-f", "lavfi", "-i", "anullsrc=r=8000:cl=stereo", *chapters,
            "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map_chapters", "3",
            "-attach", "/media/Synthetic Sans.ttf", "-metadata:s:t", "mimetype=application/x-truetype-font",
            "-metadata:s:t", "filename=Synthetic Sans.ttf",
            "-t", "600", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "35", "-c:a", "aac", "-b:a", "8k",
            "-metadata:s:a:0", "language=eng", "-metadata:s:a:1", "language=fra", "/media/multitrack.mkv"])

    def art(directory):
        Image.new("RGB", (100, 150), "orange").save(directory / "poster.jpg")
        Image.new("RGB", (200, 100), "navy").save(directory / "fanart.jpg")

    def movie(name, rating, library="movies", versions=False, template="template.mkv"):
        directory = root / library / name
        directory.mkdir(parents=True)
        names = [name + " - 1080p", name + " - 720p"] if versions else [name]
        for stem in names:
            shutil.copyfile(root / template, directory / (stem + ".mkv"))
            (directory / (stem + ".eng.srt")).write_text(
                "1\n00:00:01,000 --> 00:00:04,000\nSynthetic English subtitle.\n")
        nfo(directory / "movie.nfo", "movie", [
            ("title", name), ("year", 2020), ("plot", "An invented adoption fixture."),
            ("mpaa", rating), ("rating", 7.5), ("genre", "Adventure"),
            ("tmdbid", 990000001 + len(list(root.glob('movies/*'))) + len(list(root.glob('private/*')))),
        ], ACTORS)
        art(directory)

    movie("Amber Harbor", "G")
    movie("Blue Orchard", "PG")
    # This item gets uploaded artwork through Jellyfin's API. A competing local
    # poster would legitimately replace that upload during a local image scan.
    (root / "movies/Blue Orchard/poster.jpg").unlink()
    movie("Crimson Signal", "R")
    movie("Twin Horizon", "G", versions=True)
    movie("Hidden Meadow", "G", library="private")
    movie("Bright Lattice", "G", template="hdr10.mkv")
    movie("Quiet Chorus", "G", template="multitrack.mkv")
    # A non-ASCII path: accented Latin and CJK, NFC-encoded ("Café Ünïcode 東京").
    movie("Caf\u00e9 \u00dcn\u00efcode \u6771\u4eac", "G")
    extra = root / "movies/Amber Harbor/behind the scenes"
    extra.mkdir()
    shutil.copyfile(root / "template.mkv", extra / "Synthetic making of.mkv")

    series = root / "shows/Clockwork Garden"
    series.mkdir(parents=True)
    nfo(series / "tvshow.nfo", "tvshow", [("title", "Clockwork Garden"),
        ("plot", "An invented series."), ("mpaa", "G"), ("year", 2020), ("rating", 8)])
    art(series)
    for season in (1, 2):
        folder = series / f"Season {season:02}"
        folder.mkdir()
        nfo(folder / "season.nfo", "season", [("title", f"Season {season}"), ("seasonnumber", season)])
        for episode in (1, 2, 3):
            stem = f"Clockwork Garden S{season:02}E{episode:02}"
            shutil.copyfile(root / "template.mkv", folder / (stem + ".mkv"))
            nfo(folder / (stem + ".nfo"), "episodedetails", [
                ("title", f"Synthetic episode {season}-{episode}"), ("season", season),
                ("episode", episode), ("plot", "An invented episode."),
                ("aired", f"2020-0{season}-0{episode}"), ("rating", 8), ("mpaa", "G")], ACTORS)
            (folder / (stem + ".eng.srt")).write_text(
                "1\n00:00:01,000 --> 00:00:04,000\nSynthetic English subtitle.\n")
            art_path = folder / (stem + "-thumb.jpg")
            Image.new("RGB", (160, 90), "green").save(art_path)

    for artist, album, tracks in [
        ("Synthetic Ensemble", "First Album", [(1, 1), (1, 2), (2, 1), (2, 2)]),
        ("Synthetic Soloist", "Second Album", [(1, 1), (1, 2)]),
    ]:
        folder = root / "music" / artist / album
        folder.mkdir(parents=True)
        nfo(folder.parent / "artist.nfo", "artist", [("name", artist), ("biography", "An invented musician.")])
        nfo(folder / "album.nfo", "album", [("title", album), ("artist", artist),
            ("review", "An invented album."), ("year", 2020), ("genre", "Ambient")])
        Image.new("RGB", (100, 100), "purple").save(folder / "cover.jpg")
        for disc, track in tracks:
            target = folder / f"{disc}-{track:02} Synthetic Track.flac"
            encode(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=8000", "-t", "8",
                    "-c:a", "flac", "-metadata", f"title=Track {disc}-{track}",
                    "-metadata", f"artist={artist}", "-metadata", f"album_artist={artist}",
                    "-metadata", f"album={album}", "-metadata", f"track={track}",
                    "-metadata", f"disc={disc}", "-metadata", "date=2020", "-metadata",
                    "genre=Ambient", "/media/" + str(target.relative_to(root))])

    books = root / "books/Synthetic Author"
    books.mkdir(parents=True)
    epub(books / "Synthetic Field Notes.epub", "Synthetic Field Notes")
    photos = root / "photos/Synthetic Album"
    photos.mkdir(parents=True)
    for name, color in (("Synthetic sunrise", "gold"), ("Synthetic dusk", "indigo")):
        Image.new("RGB", (320, 240), color).save(photos / (name + ".jpg"))

    for scratch in ("template.mkv", "hdr10.mkv", "multitrack.mkv", "chapters.txt", "Synthetic Sans.ttf"):
        (root / scratch).unlink()
