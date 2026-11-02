"""Observe database writes and provider requests during an unchanged scan."""

from contextlib import contextmanager
import sqlite3
import hashlib

from metadata import CheckError


TABLES = (
    "BaseItems", "BaseItemProviders", "BaseItemImageInfos", "PeopleBaseItemMap",
    "Peoples", "MediaStreamInfos", "UserData", "LinkedChildren", "BaseItemMetadataFields",
)


@contextmanager
def audit(db):
    """Install counters on a disposable copy only; remove them even on failure.

    Row comparison alone cannot catch deleting/reinserting identical rows, or an
    UPDATE that writes the same values. Triggers count those operations too.
    """
    conn = sqlite3.connect(db, timeout=30)
    try:
        conn.execute('CREATE TABLE "AdoptionTestWrites" ("TableName" TEXT, "Operation" TEXT)')
        for table in TABLES:
            for operation in ("INSERT", "UPDATE", "DELETE"):
                row = "OLD" if operation == "DELETE" else "NEW"
                # Container scan timestamps are bookkeeping. Media, people,
                # albums/artists and every related row must remain untouched.
                condition = (f" WHEN {row}.Type NOT LIKE '%Folder' AND {row}.Type NOT LIKE '%UserView'"
                             if table == "BaseItems" else "")
                conn.execute(f'''CREATE TRIGGER "AdoptionTest_{table}_{operation}"
                    AFTER {operation} ON "{table}"{condition}
                    BEGIN INSERT INTO "AdoptionTestWrites" VALUES ('{table}', '{operation}'); END''')
        conn.commit()
        yield conn
    finally:
        for table in TABLES:
            for operation in ("INSERT", "UPDATE", "DELETE"):
                conn.execute(f'DROP TRIGGER IF EXISTS "AdoptionTest_{table}_{operation}"')
        conn.execute('DROP TABLE IF EXISTS "AdoptionTestWrites"')
        conn.commit()
        conn.close()


def assert_unchanged(conn):
    writes = conn.execute('SELECT "TableName", "Operation", count(*) FROM "AdoptionTestWrites" '
                          'GROUP BY "TableName", "Operation"').fetchall()
    if writes:
        raise CheckError("unchanged scan wrote rows: " + ", ".join(
            f"{table} {operation}={count}" for table, operation, count in writes))


def provider_requests(api):
    body = api.call("GET", "/metrics", binary=True).decode()
    return {line.split()[0]: float(line.split()[1]) for line in body.splitlines()
            if line.startswith("ferrofin_metadata_provider_requests_total{")}


def subtitle_files(roots):
    return {str(path): hashlib.sha256(path.read_bytes()).hexdigest()
            for root in roots for path in root.rglob("*")
            if path.is_file() and path.suffix.lower() in (".srt", ".vtt", ".ass", ".ssa", ".sub")}


def check(api, db, subtitle_roots=()):
    files = subtitle_files(subtitle_roots)
    before = provider_requests(api)
    with audit(db) as conn:
        api.scan(300, interval=1)
        assert_unchanged(conn)
    if files != subtitle_files(subtitle_roots):
        raise CheckError("unchanged scan changed or duplicated subtitle files")
    if before != provider_requests(api):
        raise CheckError("unchanged scan made metadata or subtitle provider requests")
