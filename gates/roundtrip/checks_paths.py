#!/usr/bin/env python3
"""The stored paths of a round trip's export, and its libraries once Jellyfin has scanned it (stdlib only).

    checks_paths.py stored --db FILE --out DIR --media DIR [--source DIR]... [--ran DIR COPY]... [--before FILE]
        on the exported database, before Jellyfin boots on it: every path it stores under Jellyfin's
        directories resolves in the export, where DIR (--out) is Jellyfin's /config, a token path through the
        directory Jellyfin expands it to, a media path under the media mount (--media is /media), and no path
        lies under a directory of the source (each --source, and both of each --ran: where the server ran, as
        its rows named it, and the copy that holds it; the first --ran its data directory, a second its
        configuration directory), in a path column or as a string value of an item's Data (a library's
        PhysicalLocationsList, a video's SubtitleFiles, ...). With --before, the database the export read (it
        needs --ran), a path the same column of the source stored without a file where it placed one (a person
        whose folder Jellyfin never made, a file gone from the media) is not the export's to resolve; an
        absolute path outside the source's directories and the media always is
    checks_paths.py libraries --before FILE --after FILE
        every library folder row of the database Ferrofin stopped with (--before) is in Jellyfin's once its
        scan ran (--after), with the same DateCreated: Jellyfin deletes a library whose stored folder it cannot
        find, and the library's playlist and collection entries with it

Exit status 0 when it holds; 1 with one line per problem on stdout, the first few of each kind.
"""

import argparse
from contextlib import closing
import json
from pathlib import Path
import re
import sqlite3
import sys

#: The Jellyfin columns that hold a path under its directories (the export's own list).
PATH_COLUMNS = (("BaseItems", "Path"), ("BaseItemImageInfos", "Path"), ("Chapters", "ImagePath"),
                ("MediaStreamInfos", "Path"), ("ImageInfos", "Path"))
#: The Jellyfin JSON columns whose string values can hold such a path (the export's own list): free text is in
#: columns of its own.
JSON_PATH_COLUMNS = (("BaseItems", "Data"),)
#: Where Jellyfin's docker image reads its program data, and the tokens it expands below it.
CONFIG = "/config"
TOKENS = {"%AppDataPath%": "data", "%MetadataPath%": "metadata"}
MEDIA = "/media"
LIBRARY = "MediaBrowser.Controller.Entities.CollectionFolder"
#: What `place` gives an absolute path outside Jellyfin's directories and the media mount.
OUTSIDE = object()
#: How many examples a problem line names.
EXAMPLES = 3


def has_table(db, name):
    """Whether the database at `db` has the table `name`."""
    return bool(rows(db, f"SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '{name}'"))


def rows(db, query):
    """The rows of `query` on the database at `db`, opened read-only; none when a table it reads is missing."""
    with closing(sqlite3.connect(f"file:{db}?mode=ro", uri=True)) as conn:
        try:
            return conn.execute(query).fetchall()
        except sqlite3.OperationalError as error:
            if "no such table" in str(error):
                return []
            raise


def place(path, roots, media):
    """Where `path` is on the host: a Path; OUTSIDE for an absolute path elsewhere, which the server's container
    never has (whatever the host holds there); or None when it names no file at all (a URL, a relative name:
    nothing to check). `roots` is [(a directory the server saw, where it is on the host)], the tokens expanding
    under the first: [(/config, the export)] for Jellyfin's docker image."""
    for token, under in TOKENS.items():
        if path == token or path.startswith(token + "/"):
            return Path(roots[0][1], under, path[len(token):].lstrip("/"))
    for root, there in (*roots, (MEDIA, media)):
        if path == root or path.startswith(root.rstrip("/") + "/"):
            return Path(there, path[len(root):].lstrip("/"))
    return OUTSIDE if path.startswith("/") else None


def json_strings(document):
    """Every string value of the JSON `document` in its order (object keys are no values); none when it is not
    JSON."""
    def strings(node):
        if isinstance(node, dict):
            node = list(node.values())
        if isinstance(node, list):
            return [value for child in node for value in strings(child)]
        return [node] if isinstance(node, str) else []
    try:
        return strings(json.loads(document))
    except ValueError:
        return []


def fileless(before, table, column, media, ran):
    """The paths `table`.`column` of the source's database stored that it placed in its directories (`ran`,
    [(directory, copy)]) or the media and that named no file there. A path it could not place is not one: the
    source held no file there only where there is a there."""
    out = set()
    for (value,) in rows(before, f'SELECT "{column}" FROM "{table}" WHERE "{column}" IS NOT NULL'):
        where = place(value, ran, media)
        if where is not None and where is not OUTSIDE and not where.exists():
            out.add(value)
    return out


def source_forms(value, ran):
    """`value` as the source may have stored it: as it is, or before the export moved its directory onto
    Jellyfin's, the first of `ran` (the data directory) onto /config, a second (the configuration) onto
    /config/config."""
    forms = {value}
    for (directory, _), target in zip(ran, (CONFIG, CONFIG + "/config")):
        if value.startswith(target + "/"):
            forms.add(directory.rstrip("/") + value[len(target):])
    return forms


def stored(db, out, media, sources, before=None, ran=()):
    """The problems of `stored` mode. A database with Jellyfin's items table and not one path in it checks
    nothing, which is a problem, not a pass. With `before`, the source's database, a path missing from the export
    that the same column of the source stored, in one of its `source_forms`, without a file (`fileless`) is left
    out."""
    if before and not ran:
        raise ValueError("the source's database needs the directories that place its paths (ran)")
    problems = []
    if has_table(db, "BaseItems") and not rows(db, 'SELECT 1 FROM "BaseItems" WHERE "Path" IS NOT NULL LIMIT 1'):
        problems.append("BaseItems.Path: no stored path to check")

    def is_under(value):
        return any(value == source or value.startswith(source.rstrip("/") + "/") for source in sources)

    for table, column in PATH_COLUMNS:
        values = [value for (value,) in rows(db, f'SELECT "{column}" FROM "{table}" WHERE "{column}" IS NOT NULL')]
        under = [value for value in values if is_under(value)]
        if under:
            problems.append(f"{table}.{column}: {len(under)} path(s) under the source's directory, first: "
                            + ", ".join(under[:EXAMPLES]))
        missing = []
        for value in values:
            where = place(value, [(CONFIG, out)], media)
            if where is OUTSIDE or (where is not None and not where.exists()):
                missing.append(value)
        if missing and before:
            without_file = fileless(before, table, column, media, ran)
            missing = [value for value in missing if not source_forms(value, ran) & without_file]
        if missing:
            problems.append(f"{table}.{column}: {len(missing)} of {len(values)} path(s) not in the export, first: "
                            + ", ".join(missing[:EXAMPLES]))
    for table, column in JSON_PATH_COLUMNS:
        if column not in [name for (_, name, *_) in rows(db, f'PRAGMA table_info("{table}")')]:
            continue
        under = [value for (document,) in rows(db, f'SELECT "{column}" FROM "{table}" WHERE "{column}" IS NOT NULL')
                 for value in json_strings(document) if is_under(value)]
        if under:
            problems.append(f"{table}.{column}: {len(under)} path(s) under the source's directory, first: "
                            + ", ".join(under[:EXAMPLES]))
    return problems


def instant(value):
    """A stored DateCreated in one form: EF Core and Ferrofin write the same instant with a `T` or a space, a
    trailing `Z` or none, and up to seven fraction digits."""
    match = re.fullmatch(r"(\d{4}-\d\d-\d\d)[T ](\d\d:\d\d:\d\d)(?:\.(\d+))?Z?", value or "")
    if not match:
        return value
    date, time, fraction = match.groups()
    return f"{date} {time}.{(fraction or '').ljust(7, '0')[:7]}"


def libraries(before, after):
    """The problems of `libraries` mode."""
    query = f"""SELECT upper("Id"), "DateCreated", "Name" FROM "BaseItems" WHERE "Type" = '{LIBRARY}'"""
    kept = {key: instant(created) for key, created, _ in rows(after, query)}
    gone, changed = [], []
    libraries_before = rows(before, query)
    if has_table(before, "BaseItems") and not libraries_before:
        return ["no library folder row before the export: nothing to check"]
    for key, created, name in libraries_before:
        if key not in kept:
            gone.append(name or key)
        elif kept[key] != instant(created):
            changed.append(f"{name or key} ({created} -> {kept[key]})")
    problems = []
    if gone:
        problems.append(f"{len(gone)} library folder row(s) gone after Jellyfin's scan: " + ", ".join(gone[:EXAMPLES]))
    if changed:
        problems.append(f"{len(changed)} library folder row(s) with another DateCreated: "
                        + ", ".join(changed[:EXAMPLES]))
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    modes = parser.add_subparsers(dest="mode", required=True)
    paths = modes.add_parser("stored")
    paths.add_argument("--db", required=True)
    paths.add_argument("--out", required=True)
    paths.add_argument("--media", required=True)
    paths.add_argument("--source", action="append", default=[])
    paths.add_argument("--before")
    paths.add_argument("--ran", nargs=2, action="append", default=[], metavar=("DIR", "COPY"))
    folders = modes.add_parser("libraries")
    folders.add_argument("--before", required=True)
    folders.add_argument("--after", required=True)
    args = parser.parse_args(argv)
    if args.mode == "stored" and args.before and not args.ran:
        parser.error("--before needs --ran: the directories that place the source's paths")
    try:
        if args.mode == "stored":
            sources = args.source + [directory for pair in args.ran for directory in pair]
            problems = stored(args.db, args.out, args.media, [s for s in sources if s.rstrip("/") != CONFIG],
                              args.before, [tuple(pair) for pair in args.ran])
        else:
            problems = libraries(args.before, args.after)
    except (OSError, sqlite3.Error) as error:
        problems = [f"cannot read the database: {type(error).__name__}: {error}"]
    for problem in problems:
        print(problem)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
