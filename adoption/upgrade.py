"""Upgrade a populated Ferrofin install from an earlier Ferrofin release to the candidate.

An earlier release (for each release: the previous release, and v1.0.0) boots on a copy of a
synthetic fixture, adopts and scans it, and gets user state through its API: a played
movie, a resume position, a favourite, a user created and one deleted. It stops, the
candidate boots on the same data directory, and the preservation checks synthetic.py run
uses compare it with what the earlier release served and stored, after the upgrade and
again after a scan. Both sides are Ferrofin, so every comparison is exact.
"""

import base64
from contextlib import closing
from io import BytesIO
import json
import re
import shutil
import sqlite3

from PIL import Image

from metadata import CheckError
from synthetic_fixture import PASSWORD, USERS, drop_dangling_chain


# The one generation every Ferrofin release adopts (v1.0.0 adopts nothing else), so one fixture
# serves every start; its copy loses the seeds no start release can boot on (unseed).
FIXTURE = "jellyfin-10.11.8"
CREATED, DELETED = "synthetic-upgrade-created", "synthetic-upgrade-deleted"
# A position no fixture seed uses, so a reset or a stale value cannot match it.
POSITION = 1_234_000_000
CONTAINER = "synthetic-adoption"  # preservation.container_log reads this name
# The API bugs of a start release that keep it from serving the strict API snapshot, where
# only its stored state is compared: v1.0.0 lets a restricted user download an item. Any
# other snapshot failure fails the run.
KNOWN_API_GAPS = ("/Download returned 200",)


def user_state(manifest):
    """The items add_user_state marks for synthetic-adult, by kind of state."""
    return {"played": manifest["rated_movie"], "position": manifest["private_movie"],
            "favourite": manifest["allowed_movie"]}


def add_user_state(admin, manifest):
    """Write the user state through the earlier release's API and read it back: a state
    the start release never stored cannot test the upgrade. Returns the manifest with the
    created user, who has an avatar like every other account (user_accounts.snapshot)."""
    adult = admin.login(USERS[1])
    items = user_state(manifest)
    adult.post(f"/UserPlayedItems/{items['played']}", userId=adult.user)
    adult.post(f"/UserItems/{items['position']}/UserData",
               {"Played": False, "PlaybackPositionTicks": POSITION}, userId=adult.user)
    adult.post(f"/UserFavoriteItems/{items['favourite']}", userId=adult.user)
    created = admin.post("/Users/New", {"Name": CREATED, "Password": PASSWORD})["Id"]
    picture = BytesIO()
    Image.new("RGB", (80, 64), "purple").save(picture, format="PNG")
    admin.call("POST", "/UserImage", raw=base64.b64encode(picture.getvalue()), userId=created)
    doomed = admin.post("/Users/New", {"Name": DELETED, "Password": PASSWORD})["Id"]
    admin.call("DELETE", f"/Users/{doomed}")
    rows = adult.get("/Items", userId=adult.user, ids=",".join(items.values()), enableUserData="true")["Items"]
    names = [user["Name"] for user in admin.get("/Users")]
    missing = state_failures({row["Id"].replace("-", "").lower(): row.get("UserData") or {} for row in rows},
                             names, items)
    if missing:
        raise CheckError("the start release did not store " + ", ".join(missing))
    return {**manifest, "users": {**manifest["users"], CREATED: created}}


def state_failures(data, names, items):
    """What of the written user state is absent: data maps item id to its UserData."""
    checks = {
        "the played movie": data.get(items["played"], {}).get("Played") is True,
        "the resume position": data.get(items["position"], {}).get("PlaybackPositionTicks") == POSITION,
        "the favourite": data.get(items["favourite"], {}).get("IsFavorite") is True,
        "the created user": CREATED in names,
        "the user deletion": DELETED not in names,
    }
    return [name for name, held in checks.items() if not held]


def boot_failures(log):
    """Why the candidate's boot on the earlier release's database failed, or nothing."""
    failures = []
    if '"database migrations applied"' not in log:
        failures.append("migrations did not complete")
    if '"level":"ERROR"' in log:
        failures.append("ERROR in boot log")
    return failures


def unseed(db, seeds):
    """Remove from a STOPPED copy the seeded shapes releases before v1.4.0 refuse to boot on:
    the dangling parent chain (no orphan clean-up: a foreign-key violation after 0032) and the
    stale LinkedChildren entries (the playlist/collection reconcile and the adoption repairs
    fail on a foreign key). Each linked container keeps its live member. synthetic.py run
    checks the candidate on both shapes; this check needs a start release that boots."""
    drop_dangling_chain(db, seeds)
    with closing(sqlite3.connect(db)) as conn:
        with conn:
            for seed in seeds["linked_children"].values():
                conn.execute('UPDATE "BaseItems" SET "Data" = json_set("Data", \'$.LinkedChildren\', json(?)) '
                             'WHERE lower(replace("Id", \'-\', \'\')) = ?', (json.dumps(seed["entries"][:1]), seed["id"]))
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")


def work_name(start, fixture):
    """A work directory name for one start image and fixture."""
    return "upgrade-" + re.sub(r"[^A-Za-z0-9.-]", "_", start.rsplit("/", 1)[-1]) + "-" + fixture


def stores(db, expected=False):
    """What the database holds, in the form each preservation check compares. The start
    release's (expected=True) history is harmonized: a release before v1.4.0 left the seeded
    conflicting rows apart, and the candidate's boot repair makes them agree, as Jellyfin 12.2
    does (synthetic.py run holds that against the 12.2 oracle)."""
    import metadata
    import user_accounts
    import watch_history
    return {"accounts": user_accounts.database_snapshot(db), "history": watch_history.snapshot(db, expected),
            "metadata": metadata.snapshot(db)}


def comparable(data, seeds):
    """An API snapshot without what the candidate's boot repairs change by design, following
    Jellyfin 12.2 rather than the start release (synthetic.py run holds each against the 12.2
    oracle): the seeded ratings' visibility (rescored from 12.2's tables), the conflicting
    movie's resume (harmonized) and the local alternate version's id (re-keyed from its
    primary's type, preservation.FROM_ORACLE), of which only the count is compared. Avatars
    are fetched with the admin's token (snapshot images=admin): v1.0.0 serves them to no
    anonymous client, and synthetic.py run checks anonymous access on the candidate."""
    from preservation import scrub
    return {**data, "visibility": scrub(data["visibility"], set(seeds["ratings"])),
            "views": scrub(data["views"], {seeds["conflict"]["item"]}), "versions": len(data["versions"])}


def differences(before, stored, served, actual, db, base):
    """Every preservation check, the candidate (stored, served) against the start release.
    served and actual are API snapshots in comparable form, or None when the start release
    could not serve one."""
    import metadata
    import user_accounts
    import watch_history
    from preservation import compare
    changed = compare(served, actual) if served is not None else []
    changed += user_accounts.compare_database(before["accounts"], stored["accounts"])
    changed += list(watch_history.compare_db(before["history"], stored["history"]))
    changed += list(metadata.compare(before["metadata"], stored["metadata"], "database"))
    api = metadata.Api(base, db)
    changed += list(watch_history.compare_api(before["history"], api))
    changed += list(metadata.compare(before["metadata"], api.snapshot(before["metadata"]), "API"))
    api.images(before["metadata"])  # raises on an unservable image
    return changed


def upgrade(root, start, image, fixture, port):
    """Upgrade one copy of `fixture` from image `start` to `image`; the failures, or []."""
    from preservation import container_log, snapshot, verify_database
    from synthetic import save, server
    source = root / fixture
    ready = source / "synthetic-ready.json"
    # No recipe check: the recipe guards the Jellyfin baselines, and this run takes both of
    # its baselines from Ferrofin itself. The manifest only names the items and users.
    if not ready.exists() or json.loads(ready.read_text()).get("fixture") != fixture:
        raise CheckError(f"synthetic fixture {fixture} is not built; run synthetic.py build first")
    manifest = json.loads((source / "synthetic.json").read_text())
    work = root / "work" / work_name(start, fixture)
    if work.exists():
        raise CheckError("previous upgrade run remains; inspect it before rerunning")
    work.parent.mkdir(exist_ok=True)
    shutil.copytree(source, work)
    db = work / "data/jellyfin.db"
    unseed(db, manifest["seeds"])
    log = work.parent / (work.name + ".server.log")
    with server(start, work, root / "media", CONTAINER, port, ferrofin=True) as api:
        admin = api.login(USERS[0])
        admin.idle()
        admin.scan(300, interval=1)
        manifest = add_user_state(admin, manifest)
        admin.idle()
        try:
            served = snapshot(admin, manifest, images=admin)
        except CheckError as error:
            # An old release's own API bug is no baseline to preserve; the stored state and
            # the candidate's API against it still are.
            if not any(gap in str(error) for gap in KNOWN_API_GAPS):
                raise
            served = None
            print(f"NOTE {start} serves no complete API snapshot ({error}); "
                  "comparing its stored state only", flush=True)
    if served is not None:
        save(work / "start-served.json", served)
    log.rename(work.parent / (work.name + ".start.server.log"))
    try:
        verify_database(db)
    except CheckError as error:
        raise CheckError(f"the start release left a broken database: {error}") from None
    before = stores(db, expected=True)
    failures = []
    with server(image, work, root / "media", CONTAINER, port, ferrofin=True) as api:
        admin = api.login(USERS[0])
        admin.idle()
        boot = boot_failures(container_log())
        if boot:
            failures.append(f"{start} boot: " + ", ".join(boot))
        for stage in ("upgrade", "scan"):
            if stage == "scan":
                admin.scan(300, interval=1)
            changed = []
            try:
                verify_database(db)
            except CheckError as error:
                changed.append(str(error))
            actual = snapshot(admin, manifest, images=admin) if served is not None else None
            changed += differences(before, stores(db), comparable(served, manifest["seeds"]) if served is not None else None,
                                  comparable(actual, manifest["seeds"]) if actual is not None else None, db, api.base)
            if changed:
                failures.append(f"{start} {stage}: " + ", ".join(changed))
                if actual is not None:
                    save(work / (stage + "-actual.json"), actual)
            print(("FAIL " if changed else "PASS ") + f"{start} -> {image} {stage}", flush=True)
    if not failures:
        shutil.rmtree(work)
        shutil.rmtree(work.parent / (work.name + "-cache"))
        for suffix in (".server.log", ".start.server.log"):
            (work.parent / (work.name + suffix)).unlink()
    return failures


def run(root, starts, image, fixture, port):
    """Upgrade from every start release in turn; raises with every failure."""
    if not starts:
        raise CheckError("upgrade needs at least one --from image")
    failures = []
    for start in starts:
        try:
            failures += upgrade(root, start, image, fixture or FIXTURE, port)
        except CheckError as error:  # one start that cannot run must not hide another's result
            failures.append(f"{start}: {error}")
    if failures:
        raise CheckError("; ".join(failures))

