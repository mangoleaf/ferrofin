"""Compare the synthetic library's behavior with its source Jellyfin release."""

from contextlib import closing
from io import BytesIO
import json
import copy
from pathlib import Path
import shutil
import subprocess
import sqlite3
import uuid

from PIL import Image, ImageStat

from metadata import CheckError, connect
from synthetic_fixture import USERS


LIBRARY_OPTIONS = ("PreferredMetadataLanguage", "MetadataCountryCode", "SubtitleDownloadLanguages",
                   "RequirePerfectSubtitleMatch", "SaveSubtitlesWithMedia", "SkipSubtitlesIfAudioTrackMatches",
                   "SkipSubtitlesIfEmbeddedSubtitlesPresent", "DisabledSubtitleFetchers",
                   "EnableRealtimeMonitor", "SaveLocalMetadata", "TypeOptions")
RELATIONS = ("Name", "Type", "MediaType", "IsFolder", "IndexNumber", "ParentIndexNumber", "SeriesId",
             "SeasonId", "AlbumId", "Album", "Artists", "AlbumArtist", "AlbumArtists", "ArtistItems")
MANUAL = ("Name", "Overview", "ForcedSortName", "LockData", "LockedFields", "Genres", "ProviderIds")
TYPES = "Movie,Series,Season,Episode,Audio,MusicAlbum,MusicArtist"


def select(row, keys):
    return {key: row.get(key) for key in keys}


def normalize(value):
    if isinstance(value, dict):
        return {key: normalize(val) for key, val in value.items()}
    if isinstance(value, (list, tuple)):
        return [normalize(val) for val in value]
    if isinstance(value, str):
        try:
            return uuid.UUID(value).hex
        except ValueError:
            pass
    return value


def ids(rows):
    return [row["Id"] for row in rows]


def artwork(api, item):
    image = api.call("GET", f"/Items/{item}/Images/Primary", binary=True, format="Png")
    with Image.open(BytesIO(image)) as picture:
        rgb = picture.convert("RGB")
        return {"size": list(rgb.size), "rgb": ImageStat.Stat(rgb).mean}


def snapshot(admin, manifest, images=None):
    """Use authenticated users for permissions and views, not an admin impersonation.
    images: the client user avatars are fetched as (user_accounts.snapshot)."""
    result = {"version": 1, "libraries": {},
              "visibility": {}, "denials": {}, "views": {}, "playlists": {}}
    from user_accounts import snapshot as account_snapshot
    result["accounts"] = account_snapshot(admin, manifest, images)
    for library in admin.get("/Library/VirtualFolders"):
        if library["Name"] in manifest["libraries"]:
            result["libraries"][library["Name"]] = select(library["LibraryOptions"], LIBRARY_OPTIONS)
    items = admin.get("/Items", userId=admin.user, recursive="true", limit=1000,
                      includeItemTypes=TYPES, fields="Path,Overview,Genres,ProviderIds,MediaSources,MediaStreams")["Items"]
    result["relationships"] = {row["Id"]: select(row, RELATIONS) for row in items}
    result["subtitles"] = {row["Id"]: sorted(
        (stream.get("Language"), stream.get("IsExternal"), stream.get("Codec"))
        for stream in row.get("MediaStreams", []) if stream.get("Type") == "Subtitle")
        for row in items if row["Type"] in ("Movie", "Episode")}
    result["music"] = {row["Id"]: select(row, (*RELATIONS, "Overview", "Genres", "ProductionYear", "ProviderIds"))
                       for row in items if row["Type"] in ("Audio", "MusicAlbum", "MusicArtist")}
    result["music_artwork"] = {row["Id"]: artwork(admin, row["Id"]) for row in items if row["Type"] == "MusicAlbum"}
    result["manual"] = select(admin.get(f"/Items/{manifest['locked_movie']}", userId=admin.user), MANUAL)
    result["custom_artwork"] = artwork(admin, manifest["locked_movie"])
    result["versions"] = sorted(source["Id"] for source in admin.get(
        f"/Items/{manifest['versions_movie']}", userId=admin.user).get("MediaSources", []))
    result["extras"] = sorted(ids(admin.get(f"/Items/{manifest['allowed_movie']}/SpecialFeatures", userId=admin.user)))
    result["collection"] = sorted(ids(admin.get("/Items", parentId=manifest["collection"], userId=admin.user)["Items"]))
    owner = admin.login(USERS[1])
    for key in ("playlist", "private_playlist"):
        playlist = manifest[key]
        data = owner.get(f"/Playlists/{playlist}")
        result["playlists"][key] = {
            "details": select(data, ("UserId", "OwnerId", "IsPublic", "Users", "Shares", "OpenAccess")),
            "shares": owner.get(f"/Playlists/{playlist}/Users"),
            "items": ids(owner.get(f"/Playlists/{playlist}/Items", userId=owner.user)["Items"]),
        }
    result["linked_children"] = {
        "collection": sorted(ids(admin.get("/Items", parentId=manifest["linked_collection"], userId=admin.user)["Items"])),
        **{role: ids(owner.get(f"/Playlists/{manifest['linked_' + role]}/Items", userId=owner.user)["Items"])
           for role in ("playlist", "pathless")},
    }
    for name in USERS[1:]:
        api = admin.login(name)
        visible = api.get("/Items", userId=api.user, recursive="true", includeItemTypes="Movie,Episode,Audio", limit=1000)["Items"]
        result["visibility"][name] = sorted(ids(visible))
        # Negative read and write checks use the restricted user's own token.
        other = manifest["users"][USERS[1] if name == USERS[2] else USERS[2]]
        result["denials"][name] = {
            "read_other_history": api.call("GET", "/Items", allowed=(401, 403),
                ids=manifest["allowed_movie"], userId=other)["status"],
            "write_other_history": api.call("POST", f"/UserItems/{manifest['allowed_movie']}/UserData",
                {"Played": True}, allowed=(401, 403), userId=other)["status"],
            "download": api.call("GET", f"/Items/{manifest['allowed_movie']}/Download", allowed=(401, 403))["status"],
            "delete": api.call("DELETE", f"/Items/{manifest['allowed_movie']}", allowed=(401, 403))["status"],
        }
        if name == USERS[2]:
            result["denials"][name]["private_playlist"] = api.call("GET",
                f"/Playlists/{manifest['private_playlist']}/Items", allowed=(401, 403, 404), userId=api.user)["status"]
            result["denials"][name]["edit_shared_playlist"] = api.call("POST",
                f"/Playlists/{manifest['playlist']}/Items", allowed=(401, 403),
                ids=result["playlists"]["playlist"]["items"][0], userId=api.user)["status"]
            result["playlists"]["playlist"]["shared_reader_items"] = ids(api.get(
                f"/Playlists/{manifest['playlist']}/Items", userId=api.user)["Items"])
        result["views"][name] = {
            "resume": ids(api.get("/UserItems/Resume", userId=api.user, mediaTypes="Video", limit=100)["Items"]),
            "next_up": ids(api.get("/Shows/NextUp", userId=api.user, limit=100)["Items"]),
            "folders": {row["Id"]: select(row.get("UserData", {}), ("Played", "UnplayedItemCount"))
                        for row in api.get("/Items", userId=api.user, recursive="true", includeItemTypes="Series,Season", limit=100)["Items"]},
        }
    return normalize(result)


def validate(data, manifest):
    """A missing test scenario is a build failure, never a silently skipped check."""
    from user_accounts import validate as validate_accounts
    validate_accounts(data["accounts"], manifest)
    m = normalize(manifest)
    child, adult = data["visibility"][USERS[2]], data["visibility"][USERS[1]]
    checks = {
        "allowed movie": m["allowed_movie"] in child,
        "parental rating restriction": m["rated_movie"] not in child and m["rated_movie"] in adult,
        "library restriction": m["private_movie"] not in child and m["private_movie"] in adult,
        "locked metadata": data["manual"]["LockData"] and bool(data["manual"]["LockedFields"]),
        "existing English subtitles": bool(data["subtitles"]) and all(
            any(row[0] == "eng" and row[1] for row in rows) for rows in data["subtitles"].values()),
        "uploaded artwork": same_artwork({"size": [100, 150], "rgb": [0, 255, 255]}, data["custom_artwork"]),
        "alternate versions": len(data["versions"]) == 2,
        "extras": len(data["extras"]) >= 1,
        "collection members": len(data["collection"]) == 2,
        "playlist members": len(data["playlists"]["playlist"]["items"]) == 3,
        "shared playlist": bool(data["playlists"]["playlist"]["shares"]),
        "music tracks": sum(row["Type"] == "Audio" for row in data["relationships"].values()) == 6,
        "music albums": sum(row["Type"] == "MusicAlbum" for row in data["relationships"].values()) == 2,
        "music artists": sum(row["Type"] == "MusicArtist" for row in data["relationships"].values()) >= 2,
        "multiple discs": any(row["Type"] == "Audio" and row["ParentIndexNumber"] == 2 for row in data["relationships"].values()),
        "resume views": all(data["views"][name]["resume"] for name in USERS[1:]),
        "next up views": all(data["views"][name]["next_up"] for name in USERS[1:]),
    }
    missing = [key for key, present in checks.items() if not present]
    if missing:
        raise CheckError("synthetic fixture lacks: " + ", ".join(missing))


def same_artwork(expected, actual):
    # JPEG/PNG decoders can round channels differently. The generated fixtures
    # are solid colors, so a tiny channel tolerance retains image identity.
    return (isinstance(actual, dict) and expected["size"] == actual.get("size")
            and len(actual.get("rgb", [])) == 3
            and all(abs(a - b) <= 3 for a, b in zip(expected["rgb"], actual["rgb"])))


# The fixture whose Jellyfin 12.2 handling of the seeds is the expectation on every path
# (built 10.11.8 -> 12.0 -> 12.1 -> 12.2, the fullest upgrade route). It never holds the
# dangling chain (12.0 cannot migrate it; synthetic.NO_ORPHAN_CLEANUP), so the chain's
# expectation is not read from it: seed_failures requires that no chain item survives and,
# where the source still held the history, that it sits on the placeholder. Only
# jellyfin-12.2-from-10's seeds.json records what real Jellyfin 12.2 did with the chain.
ORACLE = "jellyfin-12.2-from-121"
# Snapshot keys whose expectation is what Jellyfin 12.2 serves rather than what the source
# release served. Every 12.x deletes a 10.11 local alternate version stored as a Video
# (MigrateLinkedChildren.CleanupWrongTypeAlternateVersions) and its scan re-creates it under
# the id its primary's type derives (LibraryManager.ResolveAlternateVersion), so a version
# group's MediaSource ids change on every path from 10.11 and on none from 12.x.
FROM_ORACLE = ("versions",)
MOVIE_TYPE = "MediaBrowser.Controller.Entities.Movies.Movie"
PLACEHOLDER = "00000000000000000000000000000001"
# The generation each release's database adopts as: the id SET the gate matches
# (10.11.9 adds no migration; 10.11.10 adds 10.11.11's three).
GENERATIONS = {"10.11.8": "10.11.8", "10.11.9": "10.11.8", "10.11.10": "10.11.11",
               "10.11.11": "10.11.11", "12.0": "12.0.0", "12.1": "12.1.0", "12.2": "12.2.0"}


def scrub(value, ids):
    """value without the given (normalized) ids, as dict keys or list members."""
    if isinstance(value, dict):
        return {key: scrub(val, ids) for key, val in value.items() if key not in ids}
    if isinstance(value, list):
        return [scrub(val, ids) for val in value if not (isinstance(val, str) and val in ids)]
    return value


# The real 10.11.8 -> 12.2 upgrade: the direct evidence for what 12.2 serves from a 10.11 source.
UPGRADED_FROM_10 = "jellyfin-12.2-from-10"


def expectation(version, baseline, oracle, upgraded):
    """What Ferrofin must serve after adopting a source of `version` whose own release served
    `baseline`. A 10.11 source takes the FROM_ORACLE keys from the 12.2 oracle (`oracle`),
    provided the real 10.11 -> 12.2 upgrade (`upgraded`) serves the same; a 12.x source keeps
    its own release's baseline. Every stage is held to these post-scan values, including the
    pre-scan ones: Ferrofin's boot repair re-keys the version in place, where 12.x serves the
    primary alone until its scan re-creates the version."""
    if not version.startswith("10.11"):
        return baseline
    differ = [key for key in FROM_ORACLE if oracle[key] != upgraded[key]]
    if differ:
        raise CheckError(f"{UPGRADED_FROM_10} and {ORACLE} serve different {', '.join(differ)}; "
                         "rebuild the fixtures before trusting either as the expectation")
    return {**baseline, **{key: oracle[key] for key in FROM_ORACLE}}


def comparable(data, seeds, orphans):
    """A snapshot as the per-release comparison sees it. Orphaned items leave every list
    on adoption. Whether a seeded movie is visible follows Jellyfin 12.2's scores, and
    whether the conflicting movie is resumable follows 12.2's harmonize, and the linked
    containers' members follow 12.2's ResolveChildId; seed_failures compares those against
    the 12.2 oracle instead of the source release."""
    data = scrub(data, orphans)
    data.pop("linked_children", None)
    if "visibility" in data:
        data["visibility"] = scrub(data["visibility"], set(seeds["ratings"]))
    if "views" in data:
        data["views"] = scrub(data["views"], {seeds["conflict"]["item"]})
    return data


def rating_triples(conn):
    """Every movie's (OfficialRating, InheritedParentalRatingValue, InheritedParentalRatingSubValue)."""
    return {normalize(row[0]): list(row[1:]) for row in conn.execute(
        'SELECT "Id", "OfficialRating", "InheritedParentalRatingValue", "InheritedParentalRatingSubValue" '
        'FROM "BaseItems" WHERE "Type" = ?', (MOVIE_TYPE,))}


def user_data_state(conn, item, user):
    """The rows of one (item, user) by CustomDataKey: the nine columns the harmonize repair
    copies, dates as exact instants, booleans as booleans."""
    from watch_history import BOOLEANS, HARMONIZED, instant
    cursor = conn.cursor()
    cursor.row_factory = sqlite3.Row
    rows = {}
    for record in cursor.execute(
            'SELECT * FROM "UserData" WHERE lower(replace("ItemId", \'-\', \'\')) = ? '
            'AND lower(replace("UserId", \'-\', \'\')) = ?', (item, user)):
        row = {column: record[column] for column in HARMONIZED if column in record.keys()}
        row["LastPlayedDate"] = instant(row.get("LastPlayedDate"))
        for column in BOOLEANS:
            if row.get(column) is not None:
                row[column] = bool(row[column])
        rows[record["CustomDataKey"]] = row
    return rows


def orphan_history(conn, seeds):
    """Where the dangling chain's play state is: "placeholder" (detached, with a
    RetentionDate), "item" (still on an orphaned item), or None (lost or malformed)."""
    history = seeds["orphan_history"]
    row = conn.execute(
        'SELECT lower(replace("ItemId", \'-\', \'\')), "Played", "PlayCount", "RetentionDate" FROM "UserData" '
        'WHERE lower(replace("UserId", \'-\', \'\')) = ? AND "CustomDataKey" = ?',
        (history["user"], history["key"])).fetchone()
    if row is None or (row[1], row[2]) != (history["Played"], history["PlayCount"]):
        return None
    if row[0] == PLACEHOLDER:
        return "placeholder" if row[3] is not None else None
    return "item" if row[0] in seeds["orphans"] else None


def linked_members(conn, container):
    """A container's LinkedChildren rows as member ids, in order."""
    return [row[0] for row in conn.execute(
        'SELECT lower(replace("ChildId", \'-\', \'\')) FROM "LinkedChildren" '
        'WHERE lower(replace("ParentId", \'-\', \'\')) = ? ORDER BY "SortOrder"', (container,))]


def linked_state(conn, seeds):
    """Each linked container as a release left it: the Data entries a 10.11 database keeps,
    or the LinkedChildren rows 12.x migrated them into."""
    migrated = conn.execute("SELECT 1 FROM sqlite_master WHERE name = 'LinkedChildren'").fetchone()
    state = {}
    for role, seed in seeds["linked_children"].items():
        if migrated:
            state[role] = linked_members(conn, seed["id"])
        else:
            data = conn.execute('SELECT "Data" FROM "BaseItems" WHERE lower(replace("Id", \'-\', \'\')) = ?',
                                (seed["id"],)).fetchone()
            state[role] = json.loads(data[0]).get("LinkedChildren") if data and data[0] else None
    return state


def seed_state(db, seeds):
    """What of the seeds a built fixture still holds, recorded beside it (seeds.json):
    each release handles them on its own boot and scan before Ferrofin ever sees them.
    Jellyfin's post-scan clean-up (CleanDatabaseScheduledTask, HasDeadParentId) deletes
    dead-parent items, so the chain itself may already be gone."""
    with closing(connect(db)) as conn:
        triples = rating_triples(conn)
        survivors = conn.execute('SELECT lower(replace("Id", \'-\', \'\')) FROM "BaseItems"').fetchall()
        conflict = seeds["conflict"]
        return {"ratings": {movie: triples.get(movie, [None])[0] for movie in seeds["ratings"]},
                "conflict": user_data_state(conn, conflict["item"], conflict["user"]),
                "orphans": sorted({row[0] for row in survivors} & set(seeds["orphans"])),
                "history": orphan_history(conn, seeds),
                "linked_children": linked_state(conn, seeds)}


def validate_seed_state(state, seeds, version):
    """A fixture whose seeds did not survive its release cannot test Ferrofin with them."""
    if state["ratings"] != seeds["ratings"]:
        raise CheckError(f"Jellyfin {version} did not keep the seeded ratings")
    rows = list(state["conflict"].values())
    if sorted(state["conflict"]) != sorted(seeds["conflict"]["keys"]):
        raise CheckError(f"Jellyfin {version} did not keep the seeded conflicting user data")
    agree = all(row == rows[0] for row in rows)
    if version == "12.2" and not agree:
        raise CheckError("Jellyfin 12.2 did not harmonize the seeded conflicting user data")
    if version != "12.2" and agree:
        raise CheckError(f"Jellyfin {version} changed the seeded conflicting user data")
    # A 10.11 save re-caches a stale ItemId from its path, after which nothing tests it. The
    # 10.11.8 fixture is seeded stopped; every later 10.11 boot runs the startup
    # CleanupCollectionAndPlaylistPathsTask, which drops each entry without a Path and saves
    # that container; the save rewrites the rest (10.11.9 drops their cached ItemId), so only
    # their paths are left to compare.
    # Every 12.x runs the same MigrateLinkedChildren, so each must hold what it resolves to.
    for role, seed in seeds["linked_children"].items():
        held = state["linked_children"][role]
        if not version.startswith("10.11"):
            kept = held == seed["members"]
        elif version == "10.11.8" or all(entry.get("Path") for entry in seed["entries"]):
            kept = held == seed["entries"]
        else:
            kept = [entry.get("Path") for entry in held or []] == [
                entry["Path"] for entry in seed["entries"] if entry.get("Path")]
        if not kept and version.startswith("10.11"):
            raise CheckError(f"Jellyfin {version} did not keep the seeded {role} LinkedChildren")
        if not kept:
            raise CheckError(f"Jellyfin {version} did not resolve the seeded {role} LinkedChildren as expected")


def validate_oracle(db, manifest):
    """A Jellyfin 12.2 fixture can serve as the seeds' oracle only if 12.2 scored every
    seeded rating (each scores above zero; 10.11.8's tables score none of them)."""
    seeds = manifest.get("seeds")
    if not seeds:
        raise CheckError("synthetic fixture lacks the database seeds")
    with closing(connect(db)) as conn:
        triples = rating_triples(conn)
    if any(triples.get(movie, [None, None])[1] in (None, 0) for movie in seeds["ratings"]):
        raise CheckError("Jellyfin 12.2 left a seeded rating unscored")


def seed_failures(conn, actual, seeds, state, oracle_conn, oracle):
    """How the adopted database (conn) and API snapshot (actual) differ from Jellyfin 12.2's
    handling of the seeds (oracle_conn, oracle: the oracle fixture's database and baseline).
    state is what the source fixture still held (seed_state): the dangling chain's history
    and a linked container are checked only where the source still had them."""
    failures = []
    if rating_triples(conn) != rating_triples(oracle_conn):
        failures.append("seeded ratings differ from Jellyfin 12.2")
    if any((movie in actual["visibility"].get(name, [])) != (movie in oracle["visibility"].get(name, []))
           for movie in seeds["ratings"] for name in USERS[1:]):
        failures.append("seeded rating visibility differs from Jellyfin 12.2")
    seeded = [*seeds["ratings"], seeds["conflict"]["item"]]
    if any((movie in actual["views"][name]["resume"]) != (movie in oracle["views"][name]["resume"])
           for movie in seeded for name in USERS[1:]):
        failures.append("seeded movie resume differs from Jellyfin 12.2")
    conflict = seeds["conflict"]
    if (user_data_state(conn, conflict["item"], conflict["user"])
            != user_data_state(oracle_conn, conflict["item"], conflict["user"])):
        failures.append("seeded conflicting user data differs from Jellyfin 12.2")
    survivors = conn.execute('SELECT COUNT(*) FROM "BaseItems" WHERE lower(replace("Id", \'-\', \'\')) IN ({})'.format(
        ",".join("?" * len(seeds["orphans"]))), seeds["orphans"]).fetchone()[0]
    if survivors:
        failures.append("seeded orphaned items survive")
    if state["history"] is not None and orphan_history(conn, seeds) != "placeholder":
        failures.append("seeded orphaned history is not on the placeholder item")
    for role, seed in seeds["linked_children"].items():
        # Only where the source still held every seeded entry: 10.11.9+ drops the path-less
        # ones before Ferrofin ever sees them.
        if state["linked_children"][role] not in (seed["entries"], seed["members"]):
            continue
        if linked_members(conn, seed["id"]) != linked_members(oracle_conn, seed["id"]):
            failures.append(f"seeded {role} LinkedChildren differ from Jellyfin 12.2")
        if actual["linked_children"][role] != oracle["linked_children"][role]:
            failures.append(f"seeded {role} LinkedChildren are served differently from Jellyfin 12.2")
    return failures


def library_settings(value):
    result = copy.deepcopy(value)
    for library in result.values():
        for options in library.get("TypeOptions", []):
            # These Vec fields serialize as absent when empty in Ferrofin.
            # Metadata/ImageFetchers keep their distinct absent/empty meaning.
            for field in ("SimilarItemProviders", "SimilarItemProviderOrder"):
                options.setdefault(field, [])
    return result


def compare(expected, actual):
    changed = []
    for key in expected:
        if key == "libraries":
            equal = library_settings(expected[key]) == library_settings(actual.get(key, {}))
        elif key == "custom_artwork":
            equal = same_artwork(expected[key], actual.get(key))
        elif key == "music_artwork":
            received = actual.get(key, {})
            equal = (expected[key].keys() == received.keys() and all(
                same_artwork(value, received[item]) for item, value in expected[key].items()))
        else:
            equal = expected[key] == actual.get(key)
        if not equal:
            changed.append(key)
    return changed


def container_log():
    result = subprocess.run(["docker", "logs", "synthetic-adoption"],
                            capture_output=True, text=True, check=True)
    return result.stdout + result.stderr


def verify_database(db):
    with closing(connect(db)) as conn:
        if conn.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
            raise CheckError("synthetic database integrity check failed")
        if conn.execute("PRAGMA foreign_key_check").fetchone() is not None:
            raise CheckError("synthetic database has foreign-key violations")


def verify_boot(work, version, stage, previous_logs):
    log = work / (stage + ".boot.log")
    log.write_text("\n".join(container_log().splitlines()[previous_logs:]))
    generation = GENERATIONS[version]
    function = "adoption_check_boot_log" if stage == "adoption" else "adoption_second_boot_repairs"
    result = subprocess.run(["bash", "-c", 'source "$1"; "$2" "$3" "$4"', "_",
        str(Path(__file__).with_name("lib.sh")), function, str(log), generation],
        capture_output=True, text=True, check=True)
    if result.stdout.strip():
        raise CheckError("synthetic " + stage + " boot checks failed; inspect the local boot log")


def run_matrix(root, image, only, port):
    from synthetic import VERSIONS, FRESH_INSTALL, server, recipe
    import metadata
    import watch_history
    import table_digests
    import user_accounts
    failures = []
    oracle_dir = root / ORACLE
    oracle_ready = oracle_dir / "synthetic-ready.json"
    if not oracle_ready.exists() or json.loads(oracle_ready.read_text()).get("recipe") != recipe():
        raise CheckError("synthetic Jellyfin 12.2 oracle missing; build " + ORACLE + " first")
    oracle = json.loads((oracle_dir / "preservation.json").read_text())
    upgraded_ready = root / UPGRADED_FROM_10 / "synthetic-ready.json"
    if not upgraded_ready.exists() or json.loads(upgraded_ready.read_text()).get("recipe") != recipe():
        raise CheckError("synthetic Jellyfin 12.2 upgrade from 10.11 missing; build " + UPGRADED_FROM_10 + " first")
    upgraded = json.loads((root / UPGRADED_FROM_10 / "preservation.json").read_text())
    for name, version, _ in VERSIONS:
        if only and only != name:
            continue
        if name in FRESH_INSTALL:
            # A first-run install carries no seeds; adoption/run.sh validates it live.
            continue
        source = root / name
        ready = source / "synthetic-ready.json"
        if not ready.exists() or json.loads(ready.read_text()).get("fixture") != name:
            raise CheckError("synthetic fixture has no completed Jellyfin baseline")
        if json.loads(ready.read_text()).get("recipe") != recipe():
            raise CheckError("synthetic recipe changed; rebuild the fixture before testing")
        manifest = json.loads((source / "synthetic.json").read_text())
        expected = expectation(version, json.loads((source / "preservation.json").read_text()), oracle, upgraded)
        seeds = manifest.get("seeds")
        if not seeds or not (source / "seeds.json").exists():
            raise CheckError("synthetic fixture lacks the database seeds; rebuild it")
        state = json.loads((source / "seeds.json").read_text())
        work = root / "work" / name
        if work.exists():
            raise CheckError("previous synthetic run remains; inspect it before rerunning")
        work.parent.mkdir(exist_ok=True)
        shutil.copytree(source, work)
        db = work / "data/jellyfin.db"
        with closing(metadata.connect(db)) as conn:
            orphans = watch_history.orphaned_items(conn) | set(seeds["orphans"])
        history = watch_history.snapshot(db, expected=True)
        accounts_before = user_accounts.database_snapshot(db)
        tables_before = table_digests.snapshot(db)
        metadata_before = metadata.snapshot(db, expected=True)
        with server(image, work, root / "media", "synthetic-adoption", port, ferrofin=True) as api:
            api = api.login(USERS[0])
            api.idle()
            # Configure an invented key so a mistaken subtitle search really
            # reaches the provider request counter. Providers remain offline.
            api.post("/Plugins/4a3f8e216c944d17a2b80f5e9c3d7a10/Configuration",
                     {"ApiKey": "synthetic-unused-key"})
            previous_logs = 0
            for stage in ("adoption", "restart", "scan", "second scan"):
                if stage == "restart":
                    previous_logs = len(container_log().splitlines())
                    subprocess.run(["docker", "restart", "synthetic-adoption"], check=True, stdout=subprocess.DEVNULL)
                    api.ready()
                    api.idle()
                elif stage == "second scan":
                    from unchanged_scan import check
                    check(api, db, (work, work.parent / (name + "-cache"), root / "media"))
                elif stage == "scan":
                    api.scan(300, interval=1)
                verify_database(db)
                if stage in ("adoption", "restart"):
                    verify_boot(work, version, stage, previous_logs)
                actual = snapshot(api, manifest)
                changed = compare(comparable(expected, seeds, orphans), comparable(actual, seeds, orphans))
                with closing(metadata.connect(db)) as conn, \
                        closing(metadata.connect(oracle_dir / "data/jellyfin.db")) as oracle_conn:
                    changed += seed_failures(conn, actual, seeds, state, oracle_conn, oracle)
                changed += user_accounts.compare_database(accounts_before, user_accounts.database_snapshot(db))
                changed += table_digests.compare(tables_before, table_digests.snapshot(db, tables_before))
                changed += list(watch_history.compare_db(history, watch_history.snapshot(db)))
                changed += list(watch_history.compare_api(history, metadata.Api(api.base, db)))
                changed += list(metadata.compare(metadata_before, metadata.snapshot(db), "database"))
                metadata_api = metadata.Api(api.base, db)
                changed += list(metadata.compare(metadata_before, metadata_api.snapshot(metadata_before), "API"))
                metadata_api.images(metadata_before)
                if changed:
                    failures.append(name + " " + stage + ": " + ", ".join(changed))
                    from synthetic import save
                    save(work / (stage.replace(" ", "-") + "-actual.json"), actual)
                print(("FAIL " if changed else "PASS ") + name + " " + stage, flush=True)
        if not any(row.startswith(name + " ") for row in failures):
            shutil.rmtree(work)
            shutil.rmtree(work.parent / (name + "-cache"))
    if failures:
        raise CheckError("; ".join(failures))
