#!/usr/bin/env python3
"""Check that populated Jellyfin metadata survives adoption and a library scan.

The baseline contains item UUIDs and field names, never titles, paths, provider
values or credentials. Reports contain only aggregate counts and field names.
"""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from io import BytesIO
import json
import math
import os
from pathlib import Path
import sqlite3
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


FIELDS = (
    "Overview", "OriginalTitle", "Tagline", "CommunityRating", "CriticRating",
    "OfficialRating", "ProductionYear", "PremiereDate", "Genres", "Studios",
    "Tags", "ProductionLocations",
)
KINDS = {"Movie", "Series", "Episode"}
IMAGE_TYPES = (
    "Primary", "Art", "Backdrop", "Banner", "Logo", "Thumb", "Disc", "Box",
    "Screenshot", "Menu", "Chapter", "BoxRear", "Profile",
)


class CheckError(Exception):
    """An actionable failure whose message contains no library content."""


def present(value):
    if isinstance(value, str):
        return bool(value.strip()) and value.strip() not in ("[]", "{}", "null")
    return value is not None and value != [] and value != {}


def item_id(value):
    return uuid.UUID(value).hex


def connect(db):
    return sqlite3.connect(Path(db).resolve().as_uri() + "?mode=ro", uri=True)


def decodable_image(body):
    """Decode image pixels without depending on the server's video toolchain."""
    try:
        from PIL import Image
    except ImportError:
        raise CheckError("metadata image decoder unavailable: install Pillow") from None
    try:
        with Image.open(BytesIO(body)) as image:
            # open() reads headers lazily; load() must decode the actual pixels.
            image.load()
            return image.width > 0 and image.height > 0
    except (OSError, ValueError, Image.DecompressionBombError):
        return False


def snapshot(db):
    """Read all relevant rows in one SQLite snapshot, including a live WAL."""
    with closing(connect(db)) as conn:
        conn.row_factory = sqlite3.Row
        conn.execute("BEGIN")
        rows = conn.execute('SELECT "Id", "Type", ' + ", ".join(
            f'b."{field}"' for field in FIELDS) + ' FROM "BaseItems" b')
        items = {}
        for row in rows:
            kind = row["Type"].rsplit(".", 1)[-1]
            if kind in KINDS:
                items[item_id(row["Id"])] = {
                    "kind": kind,
                    "fields": [field for field in FIELDS if present(row[field])
                               and not (field == "CommunityRating" and row[field] <= 0)],
                    "providers": [], "images": [], "people": False,
                }
        for row in conn.execute('SELECT "ItemId", "ProviderId", "ProviderValue" FROM "BaseItemProviders"'):
            entry = items.get(item_id(row["ItemId"]))
            if entry is not None and present(row["ProviderValue"]):
                entry["providers"].append(row["ProviderId"].casefold())
        for row in conn.execute('SELECT "ItemId", "ImageType", "Path" FROM "BaseItemImageInfos"'):
            entry = items.get(item_id(row["ItemId"]))
            if entry is not None and present(row["Path"]):
                image_type = row["ImageType"]
                if not isinstance(image_type, int) or not 0 <= image_type < len(IMAGE_TYPES):
                    raise CheckError("unsupported image type in metadata fixture")
                entry["images"].append(IMAGE_TYPES[image_type])
        for row in conn.execute('SELECT m."ItemId" FROM "PeopleBaseItemMap" m JOIN "Peoples" p ON p."Id" = m."PeopleId" WHERE trim(p."Name") != \'\''):
            entry = items.get(item_id(row["ItemId"]))
            if entry is not None:
                entry["people"] = True
        for entry in items.values():
            for key in ("providers", "images"):
                entry[key] = sorted(set(entry[key]))
        return {"version": 1, "items": items}


def validate_baseline(data):
    if not isinstance(data, dict) or data.get("version") != 1 or not isinstance(data.get("items"), dict):
        raise CheckError("unsupported metadata baseline")
    # An empty or filename-only movie library must not make this gate pass.
    if not any(entry["kind"] == "Movie" and "Overview" in entry["fields"]
               and entry["providers"] for entry in data["items"].values()):
        raise CheckError("fixture needs a movie with an overview and a provider ID before adoption")


def compare(expected, actual, source):
    failures = Counter()
    for key, before in expected["items"].items():
        after = actual["items"].get(key)
        if after is None or after["kind"] != before["kind"]:
            failures[f"{source} missing item"] += 1
            continue
        for field in set(before["fields"]) - set(after["fields"]):
            failures[f"{source} missing {field}"] += 1
        # Do not print provider names from arbitrary fixture contents.
        if set(before["providers"]) - set(after["providers"]):
            failures[f"{source} missing ProviderIds"] += 1
        if set(before["images"]) - set(after["images"]):
            failures[f"{source} missing Images"] += 1
        if before["people"] and not after["people"]:
            failures[f"{source} missing People"] += 1
    return failures


class Api:
    def __init__(self, base, db):
        self.base = base.rstrip("/")
        with closing(connect(db)) as conn:
            row = conn.execute('SELECT "AccessToken" FROM "ApiKeys" ORDER BY "DateCreated" DESC LIMIT 1').fetchone()
            user = conn.execute('SELECT u."Id" FROM "Users" u WHERE EXISTS '
                                '(SELECT 1 FROM "Permissions" p WHERE p."UserId" = u."Id" '
                                'AND p."Kind" = 0 AND p."Value" = 1) ORDER BY u."Id" LIMIT 1').fetchone()
        if not user:
            raise CheckError("fixture has no administrator for metadata item requests")
        self.user_id = item_id(user[0])
        if not row or not row[0]:
            raise CheckError("fixture has no administrator API key")
        self.auth = f'MediaBrowser Token="{row[0]}", Client="adoption", Device="adoption", DeviceId="adoption", Version="1"'

    def request(self, path, method="GET", image=False, missing_ok=False):
        request = urllib.request.Request(self.base + path, method=method,
                                         headers={"Authorization": self.auth})
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                body = response.read()
                if image:
                    return (bool(body) and response.headers.get_content_type().startswith("image/")
                            and decodable_image(body))
                return json.loads(body) if body else None
        except urllib.error.HTTPError as error:
            code = error.code
            error.close()
            if code == 404 and missing_ok:
                return None
            raise CheckError(f"metadata HTTP request returned {code}") from None
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError):
            # URLs and error bodies can include IDs, keys or library content.
            raise CheckError("metadata HTTP request failed or returned invalid JSON") from None

    def snapshot(self, expected):
        items = {}
        ids = list(expected["items"])
        fields = "Overview,OriginalTitle,Taglines,Genres,Studios,Tags,ProductionLocations,ProviderIds,People"

        def record(row):
            images = list((row.get("ImageTags") or {}).keys())
            if row.get("BackdropImageTags"):
                images.append("Backdrop")
            items[item_id(row["Id"])] = {
                "kind": row.get("Type"),
                "fields": [field for field in FIELDS if present(row.get(
                    "Taglines" if field == "Tagline" else field))],
                "providers": [key.casefold() for key, value in
                              (row.get("ProviderIds") or {}).items() if present(value)],
                "images": images, "people": bool(row.get("People")),
            }

        for start in range(0, len(ids), 100):
            query = urllib.parse.urlencode({"Ids": ",".join(ids[start:start + 100]),
                                          "Fields": fields, "Limit": 100, "UserId": self.user_id})
            result = self.request("/Items?" + query)
            if not isinstance(result, dict) or not isinstance(result.get("Items"), list):
                raise CheckError("metadata API returned an invalid item list")
            for row in result["Items"]:
                record(row)
        # Browse lists suppress alternate versions and some virtual items.
        # Their detail routes still expose metadata; a 404 here is a real miss.
        for key in ids:
            if key not in items:
                row = self.request(f"/Users/{self.user_id}/Items/{key}", missing_ok=True)
                if row is not None:
                    record(row)
        return {"version": 1, "items": items}

    def images(self, expected):
        """Decode a stable 10% movie/series sample, plus one episode poster."""
        sample = []
        for kind in ("Movie", "Series", "Episode"):
            types = ("Primary",) if kind == "Episode" else ("Primary", "Backdrop")
            candidates = sorted(key for key, entry in expected["items"].items()
                                if entry["kind"] == kind and set(types) & set(entry["images"]))
            count = 1 if kind == "Episode" else math.ceil(len(candidates) / 10)
            # UUID order is reproducible across JSON/DB ordering and all stages.
            for key in candidates[:count]:
                for image_type in types:
                    if image_type in expected["items"][key]["images"]:
                        sample.append((key, kind, image_type))

        def check(entry):
            key, kind, image_type = entry
            if not self.request(f"/Items/{key}/Images/{image_type}/0?maxWidth=200", image=True):
                raise CheckError(f"metadata {kind} {image_type} image was empty or undecodable")

        with ThreadPoolExecutor(max_workers=4) as pool:
            # Consume the results so download/decode failures reach the caller.
            list(pool.map(check, sample))

    def scan(self, timeout, interval=2):
        deadline = time.monotonic() + timeout

        def task():
            tasks = self.request("/ScheduledTasks")
            if not isinstance(tasks, list) or any(not isinstance(row, dict) for row in tasks):
                raise CheckError("metadata API returned an invalid scheduled task list")
            found = [row for row in tasks if row.get("Key") == "RefreshLibrary"]
            if len(found) != 1:
                raise CheckError("expected one RefreshLibrary scheduled task")
            return found[0]

        def pause():
            if time.monotonic() >= deadline:
                raise CheckError("timed out waiting for a completed library scan")
            time.sleep(interval)

        before = task()
        while before.get("State") != "Idle":
            pause()
            before = task()
        previous = before.get("LastExecutionResult")
        self.request("/Library/Refresh", method="POST")
        while True:
            current = task()
            result = current.get("LastExecutionResult")
            # Idle alone could be observed before the requested task starts.
            if current.get("State") == "Idle" and result and result != previous:
                if result.get("Status") != "Completed":
                    raise CheckError("library scan did not complete successfully")
                return
            pause()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("snapshot", "check", "scan"))
    parser.add_argument("db")
    parser.add_argument("baseline")
    parser.add_argument("--base-url")
    parser.add_argument("--timeout", type=float, default=600)
    args = parser.parse_args(argv)
    try:
        if args.mode == "snapshot":
            data = snapshot(args.db)
            validate_baseline(data)
            fd = os.open(args.baseline, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "w") as output:
                os.fchmod(output.fileno(), 0o600)
                json.dump(data, output, sort_keys=True)
            return 0
        expected = json.loads(Path(args.baseline).read_text())
        validate_baseline(expected)
        if not args.base_url:
            raise CheckError("--base-url is required for metadata checks")
        api = Api(args.base_url, args.db)
        if args.mode == "scan":
            if not math.isfinite(args.timeout) or args.timeout <= 0:
                raise CheckError("scan timeout must be positive")
            api.scan(args.timeout)
        failures = compare(expected, snapshot(args.db), "database")
        failures.update(compare(expected, api.snapshot(expected), "API"))
        api.images(expected)
        for field, count in sorted(failures.items()):
            print(f"{field}: {count} item(s)")
        return int(bool(failures))
    except CheckError as error:
        print(str(error))
    except (sqlite3.Error, OSError, ValueError, KeyError, TypeError):
        print("metadata check could not read the database, baseline or API response")
    return 1


if __name__ == "__main__":
    sys.exit(main())
