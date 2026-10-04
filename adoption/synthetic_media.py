"""Small, invented adoption media. Encoding uses the Jellyfin image's FFmpeg."""

from pathlib import Path
import os
import shutil
import subprocess
from xml.sax.saxutils import escape

from PIL import Image


def nfo(path, kind, fields):
    path.write_text("<" + kind + ">\n" + "".join(
        f"  <{key}>{escape(str(value))}</{key}>\n" for key, value in fields)
        + f"</{kind}>\n")


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

    encode(["-f", "lavfi", "-i", "color=c=blue:s=320x180:r=1", "-f", "lavfi",
            "-i", "anullsrc=r=8000:cl=mono", "-t", "600", "-c:v", "libx264",
            "-preset", "ultrafast", "-crf", "35", "-c:a", "aac", "-b:a", "8k",
            "-metadata:s:a:0", "language=eng", "/media/template.mkv"])

    def art(directory):
        Image.new("RGB", (100, 150), "orange").save(directory / "poster.jpg")
        Image.new("RGB", (200, 100), "navy").save(directory / "fanart.jpg")

    def movie(name, rating, library="movies", versions=False):
        directory = root / library / name
        directory.mkdir(parents=True)
        names = [name + " - 1080p", name + " - 720p"] if versions else [name]
        for stem in names:
            shutil.copyfile(root / "template.mkv", directory / (stem + ".mkv"))
            (directory / (stem + ".eng.srt")).write_text(
                "1\n00:00:01,000 --> 00:00:04,000\nSynthetic English subtitle.\n")
        nfo(directory / "movie.nfo", "movie", [
            ("title", name), ("year", 2020), ("plot", "An invented adoption fixture."),
            ("mpaa", rating), ("rating", 7.5), ("genre", "Adventure"),
            ("tmdbid", 990000001 + len(list(root.glob('movies/*'))) + len(list(root.glob('private/*')))),
        ])
        art(directory)

    movie("Amber Harbor", "G")
    movie("Blue Orchard", "PG")
    # This item gets uploaded artwork through Jellyfin's API. A competing local
    # poster would legitimately replace that upload during a local image scan.
    (root / "movies/Blue Orchard/poster.jpg").unlink()
    movie("Crimson Signal", "R")
    movie("Twin Horizon", "G", versions=True)
    movie("Hidden Meadow", "G", library="private")
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
                ("aired", f"2020-0{season}-0{episode}"), ("rating", 8), ("mpaa", "G")])
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
    (root / "template.mkv").unlink()
