#!/usr/bin/env python3
"""The databases of a round trip (stdlib only): the export's schema and loss report, a copy of a running server's
database, what Jellyfin changed between two copies row by row, and the items its scan removed or re-added.

    checks_db.py export --db FILE --source FILE --report FILE --schema FILE
        the exported database (--db) has Jellyfin's schema (--schema, the pinned SQL) and nothing else, and every
        table of Ferrofin's that held rows in the database the export read (--source) is in its loss report
        (--report), with its count
    checks_db.py snapshot --db FILE --to FILE
        a consistent copy of the database at --db, a server running on it or not (SQLite's backup)
    checks_db.py rows --window NAME --before FILE --after FILE --out FILE
        every row of every table added, removed or changed between the two copies, raw (every column as stored:
        text byte for byte, whatever its collation; numbers by value), each explained by an allowlist.py entry for
        the window or not: FILE gets the summary and every
        unexplained row, FILE.jsonl.gz every change with its values and what explained it. A table only one copy
        has, or whose columns differ, is unexplained too
    checks_db.py counts --before FILE --after FILE
        Jellyfin's scan between the two copies left the item counts by type as they were, and removed no item and
        re-added none (another id at a removed item's path, or the same id with another DateCreated)

Exit status 0 when it holds; 1 with one line per problem on stdout, the first few of each kind; 2 when a file
cannot be read. A comparison with nothing to compare (no table, no item, no Ferrofin table) is a problem, never a
pass.
"""

import argparse
from collections import Counter
from contextlib import closing
import gzip
import json
from pathlib import Path
import sqlite3
import sys

import allowlist

#: How many examples a problem line names.
EXAMPLES = 3
ITEM = "BaseItems"
#: Tables whose rows Jellyfin re-inserts under a new key on every save, with the columns that name a row instead:
#: an image row gets Id = Guid.NewGuid() each time its item is saved (Jellyfin.Server.Implementations/Item/
#: BaseItemMapper.cs:455, MapImageToEntity). A row removed and one added with the same of these is changed in place,
#: its new Id one of the columns it changed.
NATURAL = {"BaseItemImageInfos": ("ItemId", "ImageType", "Path")}


def connect(path):
    """The database at `path`, read-only; a missing file is an error, never a new empty database."""
    if not Path(path).is_file():
        raise sqlite3.OperationalError(f"no database at {path}")
    return sqlite3.connect(read_only(path), uri=True)


def read_only(path):
    """The read-only URI of the database at `path` (as_uri quotes a path with ?, # or %)."""
    return Path(path).resolve().as_uri() + "?mode=ro"


def tables(db):
    """The names of a database's own tables (not SQLite's)."""
    return [name for (name,) in db.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")]


def shown(items):
    more = f"; and {len(items) - EXAMPLES} more" if len(items) > EXAMPLES else ""
    return ", ".join(str(item) for item in items[:EXAMPLES]) + more


# --- export -------------------------------------------------------------------------------------------------

def schema(db):
    """Every table (its columns), index (its columns, an expression as <expr>, and whether it is unique), view and
    trigger (by name), one line each, SQLite's own objects and the partial indexes' WHERE aside: what the export's own
    test compares (apps/ferrofin-server/tests/export_jellyfin.rs, schema)."""
    lines = set()
    for kind, name, table in db.execute(
            "SELECT type, name, tbl_name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'"):
        if kind == "table":
            detail = [repr(row) for row in db.execute(
                'SELECT name, upper(type), "notnull", dflt_value, pk FROM pragma_table_info(?)', (name,))]
        elif kind == "index":
            detail = [row[0] for row in db.execute(
                "SELECT coalesce(name, '<expr>') FROM pragma_index_info(?1) "
                "UNION ALL SELECT 'unique=' || \"unique\" FROM pragma_index_list(?2) WHERE name = ?1", (name, table))]
        else:
            detail = []
        lines.add(f"{kind} {name} on {table}: {detail}")
    return lines


def export(db, source, report, pinned):
    """The problems of `export` mode."""
    with open(pinned, encoding="utf-8") as handle:
        sql = handle.read()
    with closing(sqlite3.connect(":memory:")) as jellyfin, closing(connect(db)) as exported:
        jellyfin.executescript(sql)
        wanted, found = schema(jellyfin), schema(exported)
    problems = []
    if extra := sorted(found - wanted):
        problems.append(f"schema: {len(extra)} object(s) Jellyfin's schema does not have, first: {shown(extra)}")
    if missing := sorted(wanted - found):
        problems.append(f"schema: {len(missing)} object(s) of Jellyfin's schema missing, first: {shown(missing)}")
    with closing(connect(source)) as read:
        held = [(name, read.execute(f'SELECT count(*) FROM "{name}"').fetchone()[0]) for name in tables(read)
                if name.startswith("Ferrofin") or name == "_sqlx_migrations"]
    if not held:
        problems.append("the database the export read holds no table of Ferrofin's: nothing to find in the loss report")
    text = "\n" + Path(report).read_text(encoding="utf-8")
    unreported = [f"{name} ({rows})" for name, rows in held
                  if rows and f"\n  {name} ({rows} {'row' if rows == 1 else 'rows'}): " not in text]
    if unreported:
        problems.append(f"loss report: {len(unreported)} table(s) of Ferrofin's held rows and are not in it with their "
                        f"count, first: {shown(unreported)}")
    return problems


# --- snapshot -----------------------------------------------------------------------------------------------

def snapshot(db, to):
    """Copies the database at `db` to `to` (which must not exist) through SQLite's backup, in one step: a server
    writing meanwhile neither tears the copy nor waits on it beyond that step."""
    if Path(to).exists():
        raise FileExistsError(f"{to} exists")
    with closing(connect(db)) as live, closing(sqlite3.connect(to)) as copy:
        live.backup(copy)


# --- rows ---------------------------------------------------------------------------------------------------

def columns_of(db, schema_name, table):
    return [row[1] for row in db.execute(f'PRAGMA "{schema_name}".table_info("{table}")')]


def key_columns(db, table):
    """The primary key's columns, in key order; every column for a table without one."""
    info = list(db.execute(f'PRAGMA "before".table_info("{table}")'))
    keyed = sorted((row for row in info if row[5]), key=lambda row: row[5])
    return [row[1] for row in keyed] or [row[1] for row in info]


def value(raw):
    """A stored value as JSON can carry it, its type kept: bytes as {"blob": hex}."""
    return {"blob": raw.hex()} if isinstance(raw, bytes) else raw


def table_changes(db, table, columns):
    """[(change, key, changed columns, before row, after row)] of one table, rows as {column: value}. Values are
    compared as stored: text byte for byte (no column's collation), numbers by value (EXCEPT holds 1 and 1.0 equal)."""
    listed = ", ".join(f'"{c}" COLLATE BINARY' for c in columns)
    gone = db.execute(f'SELECT {listed} FROM "before"."{table}" EXCEPT SELECT {listed} FROM "after"."{table}"').fetchall()
    new = db.execute(f'SELECT {listed} FROM "after"."{table}" EXCEPT SELECT {listed} FROM "before"."{table}"').fetchall()
    keys = [columns.index(c) for c in key_columns(db, table)]

    def keyed(rows):
        out = {}
        for row in rows:
            out.setdefault(tuple(row[i] for i in keys), []).append(dict(zip(columns, row)))
        return out

    before, after = keyed(gone), keyed(new)
    changes, removed, added = [], [], []
    for key in sorted(set(before) | set(after), key=repr):
        old, fresh = before.get(key, []), after.get(key, [])
        while old and fresh:   # a key both copies hold: changed in place
            changes.append(changed_row(key, old.pop(0), fresh.pop(0), columns))
        removed += [(key, row) for row in old]
        added += [(key, row) for row in fresh]
    if table in NATURAL:   # re-inserted under a new key: paired by the columns that name the row
        names = NATURAL[table]
        waiting = {}
        for key, row in added:
            waiting.setdefault(tuple(row[c] for c in names), []).append((key, row))
        unpaired = []
        for key, row in removed:
            match = waiting.get(tuple(row[c] for c in names))
            if match:
                changes.append(changed_row(key, row, match.pop(0)[1], columns))
            else:
                unpaired.append((key, row))
        removed, added = unpaired, [pair for rows in waiting.values() for pair in rows]
    return (changes + [("removed", key, [], row, None) for key, row in removed]
            + [("added", key, [], None, row) for key, row in added])


def changed_row(key, before, after, columns):
    return ("changed", key, [c for c in columns if before[c] != after[c] or type(before[c]) is not type(after[c])],
            before, after)


def explain(rules, window, table, change, changed, before, after, item_type=None):
    """(the entries that explain one change; what none explains: the columns of a changed row, or "the row" for one
    added or removed). An added or removed row is explained by the first entry that applies. `item_type` is the
    BaseItems Type of the row's item, for a table with an ItemId (allowlist.ITEM_TYPE)."""
    row = {**(after if change == "added" else before), allowlist.ITEM_TYPE: item_type}
    used, left = [], list(changed) if change == "changed" else ["the row"]
    if not left:
        return [], ["the row"]   # a changed row whose columns all compare equal: never explained by nothing
    for rule in rules:
        if not rule.applies(window, table, change, row):
            continue
        covered = left if change != "changed" else [c for c in left if rule.covers(c, before[c], after[c])]
        if covered:
            used.append(rule)
            left = [c for c in left if c not in covered]
        if not left:
            break
    return used, left


def brief(raw, width=120):
    text = json.dumps(value(raw), ensure_ascii=False, default=value)
    return text if len(text) <= width else text[:width] + "…"


def difference(before, after):
    """How one column changed, for a reader: a JSON object (BaseItems.Data) by the keys that differ."""
    old, new = allowlist.json_object(before), allowlist.json_object(after)
    if old is None or new is None:
        return f"{brief(before)} -> {brief(after)}"
    if old == new:
        return "the same JSON object, written in another order or spacing"
    keys = sorted(set(old) | set(new))
    return "; ".join(f"{k}: {brief(old.get(k, '<none>'), 60)} -> {brief(new.get(k, '<none>'), 60)}"
                     for k in keys if old.get(k, "<none>") != new.get(k, "<none>"))


def rows(window, before, after, out, rules=allowlist.RULES):
    """The problems of `rows` mode; writes `out` and `out`.jsonl.gz."""
    problems, unexplained, groups = [], [], Counter()
    with closing(sqlite3.connect("file::memory:", uri=True)) as db:   # uri: ATTACH then reads mode=ro
        named = {}
        for side, path in (("before", before), ("after", after)):
            connect(path).close()   # a missing copy fails here, where ATTACH would make an empty one
            db.execute(f'ATTACH DATABASE ? AS "{side}"', (read_only(path),))
            named[side] = {n for (n,) in db.execute(
                f"SELECT name FROM \"{side}\".sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")}
        if not named["before"]:
            return [f"the {window}: no table in {before} to compare"]
        for side, other in (("before", "after"), ("after", "before")):
            if only := sorted(named[side] - named[other]):
                problems.append(f"the {window}: {len(only)} table(s) only {side} it, first: {shown(only)}")
        types = {}   # an item's Type, from either copy: what an entry's ITEM_TYPE names on a row of its own tables
        for side in ("before", "after"):
            if ITEM in named[side]:
                types.update((str(key).upper(), kind) for key, kind in db.execute(
                    f'SELECT "Id", "Type" FROM "{side}"."{ITEM}"'))
        with gzip.open(f"{out}.jsonl.gz", "wt", encoding="utf-8") as record:
            for table in sorted(named["before"] & named["after"]):
                columns = columns_of(db, "before", table)
                if columns != columns_of(db, "after", table):
                    problems.append(f"the {window}: {table} has other columns after it")
                    continue
                for change, key, changed, old, new in table_changes(db, table, columns):
                    used, left = explain(rules, window, table, change, changed, old, new,
                                         types.get(str((new if change == "added" else old).get("ItemId")).upper()))
                    record.write(json.dumps({
                        "window": window, "table": table, "change": change, "key": [value(k) for k in key],
                        "columns": changed, "unexplained": left,
                        "explained_by": [{"name": r.name, "kind": r.kind, "cause": r.cause} for r in used],
                        "before": old and {c: value(v) for c, v in old.items()},
                        "after": new and {c: value(v) for c, v in new.items()}}, ensure_ascii=False) + "\n")
                    group = (table, change, ",".join(changed))
                    groups[group + (("unexplained",) if left else tuple(f"{r.kind}:{r.name}" for r in used))] += 1
                    if left:
                        unexplained.append((table, change, key, left, old, new))
    lines = [f"rows changed in the {window} ({Path(before).name} -> {Path(after).name}): table, change, columns, "
             "explained by (the kind and name of each allowlist.py entry) or unexplained, how many"]
    lines += [f"  {table} {change} {cols or '-'}: {' '.join(how)} {count}"
              for (table, change, cols, *how), count in sorted(groups.items())]
    lines.append(f"unexplained ({len(unexplained)}):")
    for table, change, key, left, old, new in unexplained:
        lines.append(f"  {table} {change} {brief(list(key))}")
        if change == "changed" and all(c in old for c in left):
            lines += [f"      {c}: {difference(old[c], new[c])}" for c in left]
        else:
            lines.append(f"      {brief({c: v for c, v in (old or new).items() if v is not None}, 400)}")
    Path(out).write_text("\n".join(lines) + "\n", encoding="utf-8")
    if unexplained:
        per = Counter((table, change) for table, change, *_ in unexplained)
        problems.append(f"{len(unexplained)} row(s) the {window} changed that no allowlist entry explains: "
                        + shown([f"{table} {count} {change}" for (table, change), count in per.most_common()])
                        + f" ({Path(out).name})")
    return problems


# --- counts -------------------------------------------------------------------------------------------------

def items(path):
    """{upper-case id: (type, path, DateCreated, name)} of every item."""
    with closing(connect(path)) as db:
        return {str(key).upper(): rest for key, *rest in db.execute(
            f'SELECT "Id", "Type", "Path", "DateCreated", "Name" FROM "{ITEM}"')}


def kind(stored):
    return (stored or "?").rsplit(".", 1)[-1]


def counts(before, after):
    """The problems of `counts` mode."""
    old, new = items(before), items(after)
    if not old:
        return ["no item before the scan: nothing to count"]
    problems = []
    a, b = Counter(kind(v[0]) for v in old.values()), Counter(kind(v[0]) for v in new.values())
    if moved := [f"{t} {a[t]} -> {b[t]}" for t in sorted(set(a) | set(b)) if a[t] != b[t]]:
        problems.append(f"item counts by type changed: {shown(moved)}")
    removed = [key for key in old if key not in new]
    gone = set(removed)
    if removed:
        problems.append(f"{len(removed)} item(s) removed, first: "
                        + shown([f"{old[k][3]} ({kind(old[k][0])})" for k in removed]))
    at = {(v[0], v[1]): key for key, v in old.items() if key in gone and v[1]}
    again = [key for key, v in new.items() if key not in old and (v[0], v[1]) in at]
    again += [key for key, v in new.items() if key in old and v[2] != old[key][2]]
    if again:
        problems.append(f"{len(again)} item(s) re-added (another id at a removed item's path, or another DateCreated), "
                        "first: " + shown([f"{new[k][3]} ({kind(new[k][0])})" for k in again]))
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    modes = parser.add_subparsers(dest="mode", required=True)
    mode = modes.add_parser("export")
    for name in ("--db", "--source", "--report", "--schema"):
        mode.add_argument(name, required=True)
    mode = modes.add_parser("snapshot")
    mode.add_argument("--db", required=True)
    mode.add_argument("--to", required=True)
    mode = modes.add_parser("rows")
    mode.add_argument("--window", required=True, choices=allowlist.WINDOWS)
    for name in ("--before", "--after", "--out"):
        mode.add_argument(name, required=True)
    mode = modes.add_parser("counts")
    mode.add_argument("--before", required=True)
    mode.add_argument("--after", required=True)
    args = parser.parse_args(argv)
    try:
        if args.mode == "export":
            problems = export(args.db, args.source, args.report, args.schema)
        elif args.mode == "snapshot":
            snapshot(args.db, args.to)
            problems = []
        elif args.mode == "rows":
            problems = rows(args.window, args.before, args.after, args.out)
        else:
            problems = counts(args.before, args.after)
    except (OSError, sqlite3.Error) as error:
        print(f"cannot read or write a database: {type(error).__name__}: {error}")
        return 2
    for problem in problems:
        print(problem)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
