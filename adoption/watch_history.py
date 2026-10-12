#!/usr/bin/env python3
"""Check watch history on private fixture copies; report aggregate failures only."""

import argparse
from collections import Counter, defaultdict
from contextlib import closing
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import uuid
from urllib.parse import urlencode

from metadata import Api, CheckError, connect, item_id


FIELDS = ("Played", "PlayCount", "PlaybackPositionTicks", "LastPlayedDate", "IsFavorite")


def timestamp(value):
    # SQLite and JSON use different separators/UTC suffixes for the same instant.
    if not value:
        return None
    date = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if date.tzinfo is None:
        date = date.replace(tzinfo=timezone.utc)
    return date.astimezone(timezone.utc).isoformat()


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def state(row):
    return {field: timestamp(row.get(field)) if field == "LastPlayedDate" else row.get(field)
            for field in FIELDS}


def snapshot(db):
    with closing(connect(db)) as conn:
        conn.row_factory = sqlite3.Row
        conn.execute("BEGIN")
        kinds = {item_id(row[0]): row[1].rsplit(".", 1)[-1]
                 for row in conn.execute('SELECT "Id", "Type" FROM "BaseItems"')}
        rows = {}
        candidates = defaultdict(list)
        for record in conn.execute('SELECT * FROM "UserData"'):
            row = dict(record)
            item, user = item_id(row.pop("ItemId")), item_id(row.pop("UserId"))
            key = row.pop("CustomDataKey")
            # Hash provider-derived keys and the complete row. The baseline must
            # not reveal provider IDs or other library contents.
            rows[digest([item, user, key])] = digest(row)
            if kinds.get(item) in ("Movie", "Episode"):
                candidates[user + ":" + item].append((key, state(row)))
        items = {}
        for pair, choices in candidates.items():
            own = str(uuid.UUID(pair.split(":")[1]))
            choices.sort(key=lambda choice: (choice[0] != own, choice[0]))
            items[pair] = choices[0][1]
        return {"version": 1, "rows": rows, "items": items}


def compare_db(before, after):
    failures = Counter()
    for key, value in before["rows"].items():
        if key not in after["rows"]:
            failures["database missing watch-history row"] += 1
        elif value != after["rows"][key]:
            failures["database changed watch-history row"] += 1
    return failures


def compare_api(expected, api):
    failures = Counter()
    users = defaultdict(list)
    for pair in expected["items"]:
        user, item = pair.split(":")
        users[user].append(item)
    for user, ids in users.items():
        actual = {}
        for start in range(0, len(ids), 100):
            result = api.request("/Items?" + urlencode({
                "UserId": user, "Ids": ",".join(ids[start:start + 100]),
                "Limit": 100, "EnableUserData": "true",
            }))
            if not isinstance(result, dict) or not isinstance(result.get("Items"), list):
                raise CheckError("watch-history API returned an invalid item list")
            for row in result["Items"]:
                actual[item_id(row["Id"])] = row.get("UserData")
        for item in ids:
            # Alternate versions can be suppressed by browse lists.
            if item not in actual:
                row = api.request(f"/Users/{user}/Items/{item}", missing_ok=True)
                actual[item] = row.get("UserData") if row else None
            data = actual[item]
            if not isinstance(data, dict):
                failures["API missing watch history"] += 1
                continue
            after = state(data)
            for field, value in expected["items"][user + ":" + item].items():
                if after[field] != value:
                    failures[f"API changed {field}"] += 1
    return failures


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("snapshot", "check"))
    parser.add_argument("db")
    parser.add_argument("baseline")
    parser.add_argument("--base-url")
    args = parser.parse_args(argv)
    try:
        if args.mode == "snapshot":
            data = snapshot(args.db)
            if not any(row["Played"] for row in data["items"].values()):
                raise CheckError("fixture needs watched movie or episode history before adoption")
            if not any(row["PlaybackPositionTicks"] > 0 for row in data["items"].values()):
                raise CheckError("fixture needs movie or episode resume progress before adoption")
            fd = os.open(args.baseline, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            with os.fdopen(fd, "w") as output:
                os.fchmod(output.fileno(), 0o600)
                json.dump(data, output, sort_keys=True)
            return 0
        expected = json.loads(Path(args.baseline).read_text())
        if expected.get("version") != 1 or not args.base_url:
            raise CheckError("watch-history check needs a valid baseline and --base-url")
        failures = compare_db(expected, snapshot(args.db))
        failures.update(compare_api(expected, Api(args.base_url, args.db)))
        for field, count in sorted(failures.items()):
            print(f"{field}: {count} record(s)")
        return int(bool(failures))
    except CheckError as error:
        print(str(error))
    except (sqlite3.Error, OSError, ValueError, KeyError, TypeError):
        print("watch-history check could not read the database, baseline or API response")
    return 1


if __name__ == "__main__":
    sys.exit(main())
