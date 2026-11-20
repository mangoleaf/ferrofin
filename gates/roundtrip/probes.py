#!/usr/bin/env python3
"""Jellyfin serving the files a round trip's export carried (stdlib only): every image, chapter image, profile
image, trickplay tile, external subtitle and attachment the returned database lists answers 200 (B8).

    probes.py --base-url URL --data-dir DIR --before FILE --out FILE

What to probe is read from DIR's data/jellyfin.db while the server runs, as Jellyfin's API key (api.py):
    image       GET /Items/{item}/Images/{type}/0                  each item's each image type (BaseItemImageInfos)
    chapter     GET /Items/{item}/Images/Chapter/{index}           each chapter with an image (Chapters)
    profile     GET /UserImage?userId={user}                       each user with a profile image (ImageInfos)
    trickplay   GET /Videos/{item}/Trickplay/{width}/0.jpg         each item's each width (TrickplayInfos)
    subtitle    GET /Videos/{item}/{item}/Subtitles/{index}/Stream.{format}
                                                                   each external text subtitle (MediaStreamInfos)
    attachment  GET /Videos/{item}/{item}/Attachments/{index}      each attachment (AttachmentStreamInfos)
FILE (--out) gets one line per kind: `<kind> <answered 200>/<probed>`. The database Ferrofin stopped with
(--before) lists the same files: one Jellyfin's database no longer lists, for an item or user it still has (a
row it dropped because the file was missing), fails, so a lost class cannot pass by having nothing left to
probe. An item Jellyfin removed whole is the answers' and the library check's to judge, not the probes'. When
neither database lists a file at all and Jellyfin's has its image table, that fails too. Exit status 0 when every probe
answered 200 and no kind lost rows; 1 with a line per kind that did not (how many, and the first few), or one
line when the server could not be asked.
"""

import argparse
from contextlib import closing
from pathlib import Path
import sqlite3
import sys

import api

#: Jellyfin's ImageType, by the number BaseItemImageInfos stores.
IMAGE_TYPES = ("Primary", "Art", "Backdrop", "Banner", "Logo", "Thumb", "Disc", "Box", "Screenshot", "Menu",
               "Chapter", "BoxRear", "Profile")
#: MediaStreamType.Subtitle as MediaStreamInfos stores it.
SUBTITLE = 2
#: The subtitle format each external text codec is asked for in.
FORMATS = {"subrip": "srt", "srt": "srt", "ass": "ass", "ssa": "ssa", "webvtt": "vtt", "vtt": "vtt"}
#: How many failed probes a problem line names.
EXAMPLES = 3


def guid(value):
    """An id as the API's routes take it: 32 lower-case hex digits."""
    return value.replace("-", "").lower()


def probes(db):
    """[(kind, path, owner)] of every probe, from the database at `db` (read-only), owner being the id of the
    item or user the file belongs to; a table it lacks gives none."""
    def query(sql):
        with closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as conn:
            try:
                return conn.execute(sql).fetchall()
            except sqlite3.OperationalError as error:
                if "no such table" in str(error):
                    return []
                raise

    found = []
    for item, kind in query('SELECT DISTINCT "ItemId", "ImageType" FROM "BaseItemImageInfos" ORDER BY 1, 2'):
        if 0 <= kind < len(IMAGE_TYPES):
            found.append(("image", f"/Items/{guid(item)}/Images/{IMAGE_TYPES[kind]}/0", guid(item)))
    for item, index in query('SELECT "ItemId", "ChapterIndex" FROM "Chapters" WHERE "ImagePath" IS NOT NULL ORDER BY 1, 2'):
        found.append(("chapter", f"/Items/{guid(item)}/Images/Chapter/{index}", guid(item)))
    for (user,) in query('SELECT "UserId" FROM "ImageInfos" WHERE "UserId" IS NOT NULL ORDER BY 1'):
        found.append(("profile", f"/UserImage?userId={guid(user)}", guid(user)))
    for item, width in query('SELECT "ItemId", "Width" FROM "TrickplayInfos" ORDER BY 1, 2'):
        found.append(("trickplay", f"/Videos/{guid(item)}/Trickplay/{width}/0.jpg", guid(item)))
    for item, index, codec in query(f'''SELECT "ItemId", "StreamIndex", lower("Codec") FROM "MediaStreamInfos"
                                        WHERE "StreamType" = {SUBTITLE} AND "IsExternal" ORDER BY 1, 2'''):
        if codec in FORMATS:
            item = guid(item)
            found.append(("subtitle", f"/Videos/{item}/{item}/Subtitles/{index}/Stream.{FORMATS[codec]}", item))
    for item, index in query('SELECT "ItemId", "Index" FROM "AttachmentStreamInfos" ORDER BY 1, 2'):
        item = guid(item)
        found.append(("attachment", f"/Videos/{item}/{item}/Attachments/{index}", item))
    return found


def has_images(db):
    """Whether the database at `db` has Jellyfin's image table."""
    with closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as conn:
        return conn.execute("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'BaseItemImageInfos'") \
            .fetchone() is not None


def owners(db):
    """The ids of every item and user in the database at `db`, as `guid` writes them."""
    with closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as conn:
        return {guid(key) for (key,) in conn.execute('SELECT "Id" FROM "BaseItems" UNION SELECT "Id" FROM "Users"')}


def lost(found, before, kept):
    """A problem line per kind: the files `before` (the probes of the database before the export) lists for an
    item or user `kept` still holds that `found` no longer lists."""
    listed = {path for _, path, _ in found}
    gone = {}
    for kind, path, owner in before:
        if path not in listed and owner in kept:
            gone.setdefault(kind, []).append(path)
    return [f"{kind}: {len(paths)} listed before the export and no longer, first: " + ", ".join(paths[:EXAMPLES])
            for kind, paths in gone.items()]


def run(server, found):
    """(problems, summary lines): each probe of `found` sent to `server`."""
    kinds, failed = {}, {}
    for kind, path, _ in found:
        status, _ = server.send("GET", path)
        kinds[kind] = kinds.get(kind, 0) + 1
        if status != 200:
            failed.setdefault(kind, []).append(f"{path} {status}")
    summary = [f"{kind} {kinds[kind] - len(failed.get(kind, []))}/{kinds[kind]}" for kind in kinds]
    problems = [f"{kind}: {len(paths)} of {kinds[kind]} not served, first: " + ", ".join(paths[:EXAMPLES])
                for kind, paths in failed.items()]
    return problems, summary


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--data-dir", required=True)
    parser.add_argument("--before", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    try:
        server = api.Server(args.base_url, args.data_dir)
        found = probes(api.database(args.data_dir))
        problems, summary = run(server, found)
        before = probes(args.before)
        if before:   # a copy with no file to serve has no table to read owners from either
            problems += lost(found, before, owners(api.database(args.data_dir)))
        elif not found and has_images(api.database(args.data_dir)):
            problems.append("no file to probe: neither database lists an image, a trickplay tile, a subtitle "
                            "or an attachment")
    except api.CheckError as error:
        print(error)
        return 1
    except (OSError, ValueError, sqlite3.Error) as error:
        print(f"cannot reach the server or read the copy: {type(error).__name__}: {error}")
        return 1
    Path(args.out).write_text("".join(f"{line}\n" for line in summary))
    for problem in problems:
        print(problem)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
