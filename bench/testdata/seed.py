#!/usr/bin/env python3
"""Seed a fresh Jellyfin 10.11.8 over its own API so the resulting config dir is the
benchmark test data (PLAN_BENCHMARK_V3 §3.2). Run by build.sh; stdlib only.

    seed.py URL OUT_JSON

Startup wizard → libraries (remote fetchers off, trickplay/chapter images off) → full
scan → users (bench = admin, viewer) → user data (played / favorites / 60 resume
positions / ratings) → wait for every scheduled task to go idle → OUT_JSON with the ids
and the persisted bench token that run.sh, screens.js and ttfs.py use.
"""

import json
import random
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

AUTH = 'MediaBrowser Client="bench", Device="bench", DeviceId="bench-seed", Version="3"'
LIBS = [("Movies", "movies", "/media/movies"), ("Shows", "tvshows", "/media/shows"), ("Music", "music", "/media/music")]
TYPES = ["Movie", "Series", "Season", "Episode", "MusicAlbum", "MusicArtist", "Audio", "MusicVideo",
         "Video", "Trailer", "BoxSet", "Person", "Studio", "Genre", "Book", "Photo"]
RESUME = 60
TICK = 10_000_000  # ticks per second


class Api:
    def __init__(self, url):
        self.url = url.rstrip("/")
        self.token = None

    def call(self, method, path, body=None, **q):
        q = {k: v for k, v in q.items() if v is not None}
        u = self.url + path + ("?" + urllib.parse.urlencode(q, doseq=True) if q else "")
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(u, data=data, method=method)
        req.add_header("Authorization", AUTH + (f', Token="{self.token}"' if self.token else ""))
        if data is not None:
            req.add_header("Content-Type", "application/json")
        for attempt in range(120):
            try:
                with urllib.request.urlopen(req, timeout=600) as r:
                    raw = r.read()
                    return json.loads(raw) if raw.strip() else None
            except urllib.error.HTTPError as e:
                if e.code != 503:  # 503 = still starting up
                    raise
                time.sleep(1)
        raise RuntimeError(f"{method} {path}: 503 for 120s")

    def get(self, path, **q):
        return self.call("GET", path, **q)

    def post(self, path, body=None, **q):
        return self.call("POST", path, body, **q)


def wait_ready(api, secs=300):
    for _ in range(secs * 2):
        try:
            api.get("/System/Info/Public")
            return
        except (urllib.error.URLError, ConnectionError, OSError):
            time.sleep(0.5)
    sys.exit("server never became ready")


def running_tasks(api):
    return [t["Name"] for t in api.get("/ScheduledTasks") if t.get("State") != "Idle"]


def drain(api, settle=30, label=""):
    """Every scheduled task idle, then a settle margin — the run.sh rule applied at build."""
    t0 = time.time()
    while True:
        busy = running_tasks(api)
        if not busy:
            break
        print(f"  {label}waiting for {busy} ({int(time.time() - t0)}s)", flush=True)
        time.sleep(10)
    time.sleep(settle)


def main():
    url, out = sys.argv[1], sys.argv[2]
    api = Api(url)
    wait_ready(api)
    print("wizard", flush=True)
    api.post("/Startup/Configuration", {"UICulture": "en-US", "MetadataCountryCode": "US", "PreferredMetadataLanguage": "en"})
    api.get("/Startup/User")
    api.post("/Startup/User", {"Name": "bench", "Password": "bench"})
    api.post("/Startup/RemoteAccess", {"EnableRemoteAccess": True, "EnableAutomaticPortMapping": False})
    api.post("/Startup/Complete")
    auth = api.post("/Users/AuthenticateByName", {"Username": "bench", "Pw": "bench"})
    api.token = auth["AccessToken"]
    uid = auth["User"]["Id"]

    print("libraries", flush=True)
    opts = {"EnableChapterImageExtraction": False, "ExtractChapterImagesDuringLibraryScan": False,
            "EnableTrickplayImageExtraction": False, "ExtractTrickplayImagesDuringLibraryScan": False,
            "SaveLocalMetadata": False, "MetadataSavers": [], "EnableRealtimeMonitor": False, "AutomaticRefreshIntervalDays": 0,
            "EnableAutomaticSeriesGrouping": False, "EnableLUFSScan": False,
            "TypeOptions": [{"Type": t, "MetadataFetchers": [], "ImageFetchers": []} for t in TYPES]}
    for name, ctype, path in LIBS:
        api.post("/Library/VirtualFolders", {"LibraryOptions": {**opts, "PathInfos": [{"Path": path}]}},
                 name=name, collectionType=ctype, paths=[path], refreshLibrary=False)
    api.post("/Library/Refresh")
    time.sleep(5)  # let the scan task leave Idle before drain() looks at it
    drain(api, settle=5, label="scan: ")

    print("users", flush=True)
    viewer = api.post("/Users/New", {"Name": "viewer", "Password": "viewer"})["Id"]

    print("user data", flush=True)
    rng = random.Random(7)
    # Sorted by id before any rng draw: Jellyfin's SortName order has ties (scan-insertion
    # order, which varies per build), and a rebuild must seed the same items.
    movies = sorted(api.get("/Items", userId=uid, recursive="true", includeItemTypes="Movie", limit=100000, fields="MediaSources")["Items"], key=lambda m: m["Id"])
    series = sorted(api.get("/Items", userId=uid, recursive="true", includeItemTypes="Series", limit=100000)["Items"], key=lambda m: m["Id"])
    episodes = sorted(api.get("/Items", userId=uid, recursive="true", includeItemTypes="Episode", limit=100000)["Items"], key=lambda m: m["Id"])
    watched_series = {s["Id"] for s in rng.sample(series, min(40, len(series)))}
    played = [m["Id"] for m in movies if rng.random() < 0.30]
    played += [e["Id"] for e in episodes if e.get("SeriesId") in watched_series and rng.random() < 0.60]
    for i, iid in enumerate(played):
        api.post(f"/UserPlayedItems/{iid}", userId=uid, datePlayed=f"2026-0{1 + i % 8}-{1 + i % 27:02d}T20:00:00Z")
    for m in movies:
        if rng.random() < 0.05:
            api.post(f"/UserFavoriteItems/{m['Id']}", userId=uid)
    watched_eps = [e for e in episodes if e.get("SeriesId") in watched_series]
    for it in rng.sample(movies, min(RESUME // 2, len(movies))) + rng.sample(watched_eps, min(RESUME // 2, len(watched_eps))):
        rt = it.get("RunTimeTicks") or 5 * TICK
        api.post(f"/UserItems/{it['Id']}/UserData", {"PlaybackPositionTicks": int(rt * rng.uniform(0.2, 0.8)), "Played": False}, userId=uid)
    for m in rng.sample(movies, min(200, len(movies))):
        api.post(f"/UserItems/{m['Id']}/UserData", {"Likes": rng.random() < 0.7}, userId=uid)
    for m in movies:
        if rng.random() < 0.10:
            api.post(f"/UserPlayedItems/{m['Id']}", userId=viewer)

    print("drain", flush=True)
    drain(api, label="tasks: ")

    print("ids", flush=True)
    views = {v["CollectionType"]: v["Id"] for v in api.get("/UserViews", userId=uid)["Items"]}
    with_images = [m for m in movies if m.get("ImageTags", {}).get("Primary") and m.get("BackdropImageTags")]
    # single-source only, so the C2 pick is never a C1 multi-version movie whose first source is SDR
    hdr = [m for m in movies if len(m.get("MediaSources", [])) == 1
           and any(s.get("VideoRange") == "HDR" for s in m["MediaSources"][0].get("MediaStreams", []))]
    stream = api.get("/Items", userId=uid, recursive="true", includeItemTypes="Movie", searchTerm="Bench Stream", fields="MediaSources")["Items"][0]
    ser = series[0]
    season = api.get(f"/Shows/{ser['Id']}/Seasons", userId=uid)["Items"][0]
    episode = api.get(f"/Shows/{ser['Id']}/Episodes", userId=uid, seasonId=season["Id"])["Items"][0]
    ids = {"user": uid, "user_name": "bench", "password": "bench", "token": api.token, "viewer": viewer,
           "views": views, "movies_view": views["movies"], "shows_view": views["tvshows"], "music_view": views["music"],
           "movie": with_images[0]["Id"], "hdr_movie": (hdr or with_images)[0]["Id"],
           "series": ser["Id"], "season": season["Id"], "episode": episode["Id"],
           "stream": stream["Id"], "stream_source": stream["MediaSources"][0]["Id"],
           "counts": {"movies": len(movies), "series": len(series), "episodes": len(episodes), "hdr": len(hdr)}}
    json.dump(ids, open(out, "w"), indent=2)
    print(json.dumps(ids["counts"]))


if __name__ == "__main__":
    main()
