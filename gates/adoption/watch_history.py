#!/usr/bin/env python3
"""Check watch history on private fixture copies; report aggregate failures only."""

import argparse
from collections import Counter, defaultdict
from contextlib import closing
from datetime import datetime, timezone
from functools import cmp_to_key
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import sys
import uuid
from urllib.parse import urlencode

from metadata import Api, CheckError, connect, item_id, orphaned_items


FIELDS = ("Played", "PlayCount", "PlaybackPositionTicks", "LastPlayedDate", "IsFavorite")
# Jellyfin's placeholder item: detached history (the retention mirror), never harmonized.
PLACEHOLDER = item_id("00000000-0000-0000-0000-000000000001")
# The nine columns the harmonize repair copies from a group's winner onto its other rows
# (adoption_repairs.rs harmonize_groups; upstream HarmonizeConflictingUserData.cs:108-119).
HARMONIZED = ("AudioStreamIndex", "IsFavorite", "LastPlayedDate", "Likes", "PlaybackPositionTicks",
              "PlayCount", "Played", "Rating", "SubtitleStreamIndex")
# Bound as Rust booleans, so a copied value is stored as 0/1.
BOOLEANS = ("IsFavorite", "Likes", "Played")
DATE = re.compile(r"(\d{4}-\d{2}-\d{2})[ T](\d{2}:\d{2}:\d{2})(?:\.(\d+))?(Z|[+-]\d{2}:?\d{2})?")


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


def instant(value):
    """LastPlayedDate as the harmonize repair orders it: nanoseconds since the epoch, UTC.

    timestamp() keeps only microseconds (Python's datetime): enough for the API
    comparison, not for the repair, which orders exact instants (Rust's DateTime<Utc>),
    so two dates one tick (100 ns) apart are different plays.
    """
    if value is None:
        return None
    match = DATE.fullmatch(value.strip())
    if not match:
        raise ValueError("unsupported LastPlayedDate format")
    day, time, fraction, zone = match.groups()
    zone = "+00:00" if zone in (None, "Z") else zone
    whole = datetime.fromisoformat(f"{day}T{time}{zone}")
    return int(whole.timestamp()) * 1_000_000_000 + int(((fraction or "") + "0" * 9)[:9])


def tick_text(value):
    """A date as the repair's conflict query compares it (adoption_repairs.rs
    CONFLICTING_GROUPS): the stored text padded to seven fraction digits, so EF's trimmed
    "…09.533" and Ferrofin's "…09.5330000" are one instant, exact to the tick."""
    if value is None:
        return None
    return value + ".0000000" if "." not in value else (value + "0000000")[:27]


def conflicting(rows):
    """Whether the repair's query selects this (item, user) group: more than one row, and
    MIN <> MAX (NULLs skipped) on any of the five watch fields."""
    if len(rows) < 2:
        return False
    for field in FIELDS:
        values = {tick_text(row.get(field)) if field == "LastPlayedDate" else row.get(field)
                  for row in rows}
        if len(values - {None}) > 1:
            return True
    return False


def winner_order(a, b):
    """The repair's winner rule (adoption_repairs.rs winner_order), greatest wins:
    LastPlayedDate (NULL last), PlayCount, PlaybackPositionTicks, then the lower
    CustomDataKey."""
    def ranked(row):
        date = instant(row.get("LastPlayedDate"))
        return tuple((value is not None, value or 0) for value in (
            date, row.get("PlayCount"), row.get("PlaybackPositionTicks")))
    left, right = ranked(a), ranked(b)
    if left != right:
        return 1 if left > right else -1
    return (b["CustomDataKey"] > a["CustomDataKey"]) - (b["CustomDataKey"] < a["CustomDataKey"])


def harmonize(rows):
    """The state the one-time harmonize repair leaves an (item, user) group in: when the
    group conflicts, every row but the winner takes the winner's nine columns, the date
    as its stored text. CustomDataKey and RetentionDate stay."""
    if not conflicting(rows):
        return rows
    winner = max(rows, key=cmp_to_key(winner_order))
    copied = {column: winner[column] for column in HARMONIZED if column in winner}
    for column in BOOLEANS:
        if copied.get(column) is not None:
            copied[column] = int(bool(copied[column]))
    return [row if row is winner else {**row, **copied} for row in rows]


def snapshot(db, expected=False):
    """Watch history as stored, or (expected=True, for a pre-adoption baseline) as
    adoption must leave it.

    Two boot-time changes are expected, not losses: the rows of an orphaned item move
    to the placeholder item with a RetentionDate (migration 0042), and a conflicting
    group is harmonized once (adoption_repairs.rs). Only the baseline models them: the
    adopted database is read as stored, so a row that later diverges from its group
    still fails. A moved row's RetentionDate is the boot time, so for those keys only
    ("moved") compare_db checks that one is set ("detached", the stored rows with the
    date reduced to set/unset); every other row, placeholder rows included, must keep
    its exact values.
    """
    with closing(connect(db)) as conn:
        conn.row_factory = sqlite3.Row
        conn.execute("BEGIN")
        kinds = {item_id(row[0]): row[1].rsplit(".", 1)[-1]
                 for row in conn.execute('SELECT "Id", "Type" FROM "BaseItems"')}
        orphans = orphaned_items(conn) if expected else set()
        groups, detached, moved = defaultdict(list), {}, {}
        for record in conn.execute('SELECT rowid AS "__rowid", * FROM "UserData" ORDER BY rowid'):
            row = dict(record)
            del row["__rowid"]
            item, user = item_id(row.pop("ItemId")), item_id(row.pop("UserId"))
            if item == PLACEHOLDER:
                detached[(user, row["CustomDataKey"])] = row
            elif item in orphans:
                # 0042 keeps the first row (lowest rowid) per (user, key), replacing a
                # placeholder row with the same key.
                moved.setdefault((user, row["CustomDataKey"]), {**row, "RetentionDate": True})
            else:
                groups[(item, user)].append(row)
        detached.update(moved)
        for (user, _), row in detached.items():
            groups[(PLACEHOLDER, user)].append(row)
        rows, retained = {}, {}
        candidates = defaultdict(list)
        for (item, user), group in groups.items():
            for row in harmonize(group) if expected and item != PLACEHOLDER else group:
                row = dict(row)
                key = row.pop("CustomDataKey")
                # Hash provider-derived keys and the complete row. The baseline must
                # not reveal provider IDs or other library contents.
                name = digest([item, user, key])
                rows[name] = digest(row)
                if item == PLACEHOLDER and not expected:
                    retained[name] = digest({**row, "RetentionDate": row.get("RetentionDate") is not None})
                if kinds.get(item) in ("Movie", "Episode"):
                    candidates[user + ":" + item].append((key, state(row)))
        items = {}
        for pair, choices in candidates.items():
            own = str(uuid.UUID(pair.split(":")[1]))
            choices.sort(key=lambda choice: (choice[0] != own, choice[0]))
            items[pair] = choices[0][1]
        result = {"version": 1, "rows": rows, "items": items}
        if expected:
            result["moved"] = sorted(digest([PLACEHOLDER, user, key]) for user, key in moved)
        else:
            result["detached"] = retained
        return result


def compare_db(before, after):
    failures = Counter()
    moved = set(before.get("moved", ()))
    for key, value in before["rows"].items():
        stored = after.get("detached", {}) if key in moved else after["rows"]
        if key not in stored:
            failures["database missing watch-history row"] += 1
        elif value != stored[key]:
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
            data = snapshot(args.db, expected=True)
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
