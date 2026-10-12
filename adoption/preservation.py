"""Compare the synthetic library's behavior with its source Jellyfin release."""

from io import BytesIO
import json
import copy
from pathlib import Path
import shutil
import subprocess
import sqlite3
import uuid

from PIL import Image, ImageStat

from metadata import CheckError
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


def snapshot(admin, manifest):
    """Use authenticated users for permissions and views, not an admin impersonation."""
    result = {"version": 1, "libraries": {},
              "visibility": {}, "denials": {}, "views": {}, "playlists": {}}
    from user_accounts import snapshot as account_snapshot
    result["accounts"] = account_snapshot(admin, manifest)
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
    with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
        if conn.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
            raise CheckError("synthetic database integrity check failed")
        if conn.execute("PRAGMA foreign_key_check").fetchone() is not None:
            raise CheckError("synthetic database has foreign-key violations")


def verify_boot(work, name, stage, previous_logs):
    log = work / (stage + ".boot.log")
    log.write_text("\n".join(container_log().splitlines()[previous_logs:]))
    generation = ("12.1.0" if "12.1" in name else "12.0.0" if "12.0" in name
                  else "10.11.8" if name.endswith((".8", ".9")) else "10.11.11")
    function = "adoption_check_boot_log" if stage == "adoption" else "adoption_second_boot_repairs"
    result = subprocess.run(["bash", "-c", 'source "$1"; "$2" "$3" "$4"', "_",
        str(Path(__file__).with_name("lib.sh")), function, str(log), generation],
        capture_output=True, text=True, check=True)
    if result.stdout.strip():
        raise CheckError("synthetic " + stage + " boot checks failed; inspect the local boot log")


def run_matrix(root, image, only, port):
    from synthetic import VERSIONS, server, recipe
    import metadata
    import watch_history
    import user_accounts
    failures = []
    for name, _, _ in VERSIONS:
        if only and only != name:
            continue
        source = root / name
        ready = source / "synthetic-ready.json"
        if not ready.exists() or json.loads(ready.read_text()).get("fixture") != name:
            raise CheckError("synthetic fixture has no completed Jellyfin baseline")
        if json.loads(ready.read_text()).get("recipe") != recipe():
            raise CheckError("synthetic recipe changed; rebuild the fixture before testing")
        manifest = json.loads((source / "synthetic.json").read_text())
        expected = json.loads((source / "preservation.json").read_text())
        work = root / "work" / name
        if work.exists():
            raise CheckError("previous synthetic run remains; inspect it before rerunning")
        work.parent.mkdir(exist_ok=True)
        shutil.copytree(source, work)
        db = work / "data/jellyfin.db"
        history = watch_history.snapshot(db)
        accounts_before = user_accounts.database_snapshot(db)
        metadata_before = metadata.snapshot(db)
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
                    verify_boot(work, name, stage, previous_logs)
                actual = snapshot(api, manifest)
                changed = compare(expected, actual)
                changed += user_accounts.compare_database(accounts_before, user_accounts.database_snapshot(db))
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
