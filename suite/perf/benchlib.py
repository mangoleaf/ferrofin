#!/usr/bin/env python3
"""Shared bring-up + request helpers for the Python bench legs.

Port of the retired ``bench-lib.js``: the fiddly first-boot + provisioning
sequence — with all its 10.11 gotchas (modern auth grammar only, JSON body
required, startup-wizard race) — lives here ONCE so the legs can't drift.
Every function takes the target's base URL explicitly, so one module drives
both servers. stdlib ``urllib`` only, matching the rest of the suite's Python.

None of this is measurement hot-path: the measured windows are driven by
vegeta (see vegeta.py); this module only provisions, warms, and probes.
"""

import http.client
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request

USER = os.environ.get("BENCH_ADMIN_USER", "bench")
PASS = os.environ.get("BENCH_ADMIN_PASSWORD", "benchpass123")

# The playlist the playlist_* rows measure. Named, not id'd, so enrich_context
# can find an existing one instead of creating a second every time it runs.
PLAYLIST_NAME = "bench-playlist"

# Modern `Authorization: MediaBrowser …` grammar only: 10.11 ships
# EnableLegacyAuthorization=false, so X-Emby-Token/X-Emby-Authorization are
# rejected by a fresh install of either server.
CLIENT_ID = 'Client="bench", Device="bench", DeviceId="bench", Version="1.0"'


def token_headers(token):
    """The auth + content-type headers every provisioned request sends."""
    return {
        "Authorization": f'MediaBrowser Token="{token}", {CLIENT_ID}',
        "Content-Type": "application/json",
    }


def request(method, url, body=None, headers=None, timeout=30):
    """One HTTP request → (status, body_bytes). Non-2xx is a normal return,
    never an exception — callers decide what a failure means."""
    req = urllib.request.Request(url, method=method, headers=headers or {})
    data = json.dumps(body).encode() if isinstance(body, (dict, list)) else body
    try:
        with urllib.request.urlopen(req, data=data, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except (urllib.error.URLError, OSError):
        return 0, b""


def get_json(url, headers=None, timeout=30):
    """GET returning parsed JSON, or None on any failure."""
    status, body = request("GET", url, headers=headers, timeout=timeout)
    if status != 200:
        return None
    try:
        return json.loads(body)
    except ValueError:
        return None


def render_body(body, ctx):
    """Fill an endpoints.py body template: string leaves are format strings
    over the run context; everything else passes through unchanged."""
    if isinstance(body, dict):
        return {k: render_body(v, ctx) for k, v in body.items()}
    if isinstance(body, list):
        return [render_body(v, ctx) for v in body]
    if isinstance(body, str):
        return body.format(**ctx)
    return body


def render_path(e, ctx):
    """An endpoint's concrete URL path for this run's context."""
    return e["path"].format(**ctx)


def fire(base, e, ctx, timeout=30):
    """One request for any ENDPOINTS entry — method/body/auth aware, so every
    leg (compare, phases, gate, warmup) drives GET and write rows identically."""
    headers = token_headers(ctx["token"]) if e["auth"] else {"Content-Type": "application/json"}
    body = render_body(e["body"], ctx) if e["body"] is not None else None
    return request(e["method"], f"{base}{render_path(e, ctx)}", body, headers, timeout)


class PooledClient:
    """One persistent HTTP/1.1 keep-alive connection, for the CLOSED-LOOP legs
    (phase C/D, pool sweep).

    ``urllib.request`` opens a fresh TCP connection per request; at 50 VU
    threads that makes the *client* the contended resource and the numbers
    measure connect overhead, not the server (k6 pooled by default, so the
    ported legs must too — review finding, round 1). ``http.client`` is not
    thread-safe: create ONE instance per thread.

    Same return contract as :func:`request`: ``(status, body_bytes)``;
    transport errors return ``(0, b"")`` after one reconnect attempt (a server
    closing an idle keep-alive connection is normal, not an error).
    """

    def __init__(self, base, timeout=30):
        u = urllib.parse.urlsplit(base)
        self._https = u.scheme == "https"
        self._host = u.hostname
        self._port = u.port or (443 if self._https else 80)
        self._timeout = timeout
        self._conn = None

    def _connect(self):
        cls = http.client.HTTPSConnection if self._https else http.client.HTTPConnection
        self._conn = cls(self._host, self._port, timeout=self._timeout)

    def close(self):
        if self._conn is not None:
            try:
                self._conn.close()
            except OSError:
                pass
            self._conn = None

    def request(self, method, url, body=None, headers=None):
        """One request over the pooled connection. `url` may be a bare
        path(+query) or an absolute URL (the host part is ignored — this
        connection is pinned to its base)."""
        if url.startswith("http"):
            u = urllib.parse.urlsplit(url)
            url = u.path + (f"?{u.query}" if u.query else "")
        data = json.dumps(body).encode() if isinstance(body, (dict, list)) else body
        for attempt in (0, 1):
            try:
                if self._conn is None:
                    self._connect()
                self._conn.request(method, url, data, headers or {})
                r = self._conn.getresponse()
                return r.status, r.read()
            except (http.client.HTTPException, OSError):
                self.close()
                if attempt:
                    return 0, b""
        return 0, b""  # pragma: no cover - loop always returns

    def fire(self, e, ctx):
        """benchlib.fire, over the pooled connection."""
        headers = token_headers(ctx["token"]) if e["auth"] else {"Content-Type": "application/json"}
        body = render_body(e["body"], ctx) if e["body"] is not None else None
        return self.request(e["method"], render_path(e, ctx), body, headers)


def authenticate(base, target):
    """Login as the bench admin → {'token', 'userId'}; raises on failure.

    Retries while the server is merely *answering* rather than ready. Jellyfin
    10.11 binds a stub Kestrel (ServerSetupApp.SetupServer) that serves
    /System/Info/Public and the whole /Startup/* wizard before the real
    ApplicationHost takes over the socket — so run.sh's first-200 coldstart
    probe and jellyfin_first_run_wizard above can both pass against the stub,
    and the handover then drops connections. Observed live: publish run 2 died
    on `auth failed: 0` one second after a "cold-start: 1.0s". Connection
    errors and 503 retry until BENCH_COLD_READY_TIMEOUT_SECS; a real rejection
    (401/…) still raises at once, so a genuinely broken login stays loud.
    """
    deadline = time.monotonic() + float(os.environ.get("BENCH_COLD_READY_TIMEOUT_SECS", 120))
    while True:
        status, body = request(
            "POST", f"{base}/Users/AuthenticateByName",
            {"Username": USER, "Pw": PASS},
            {"Content-Type": "application/json", "Authorization": f"MediaBrowser {CLIENT_ID}"},
        )
        if status == 200:
            b = json.loads(body)
            return {"token": b["AccessToken"], "userId": b["User"]["Id"]}
        if status not in (0, 503) or time.monotonic() >= deadline:
            raise RuntimeError(f"[{target}] auth failed: {status} {body[:200]!r}")
        time.sleep(1)


def item_count(base, ctx, include_types=None):
    """Recursive item count; `include_types` omitted ⇒ ALL types. Progress
    polling must use the unfiltered count: it climbs steadily as rows are
    indexed, whereas the Movie,Episode-filtered count lags on Ferrofin (items
    are classified late in the scan) and can sit flat ~20s during a single
    slow 4K ffprobe — which used to make wait_for_scan settle prematurely."""
    t = f"&includeItemTypes={include_types}" if include_types else ""
    j = get_json(f"{base}/Items?userId={ctx['userId']}&recursive=true{t}&limit=0",
                 token_headers(ctx["token"]))
    return j.get("TotalRecordCount", 0) if j else -1


def wait_for_scan(base, target, ctx):
    """Poll the unfiltered total until it stops growing, then report the
    Movie,Episode count. Shared, API-defined completion signal — no
    scan-status API needed, works identically on both servers."""
    last, stable, zeros = -1, 0, 0
    for _ in range(480):  # 480*5s = 40min cap
        n = item_count(base, ctx)
        print(f"[{target}] scan progress: {n} items", flush=True)
        # Settle only after the total holds ~40s (8 polls): a single large 4K
        # ffprobe can pause growth ~20s, so a shorter window false-settles.
        stable = stable + 1 if (n == last and n > 0) else 0
        if stable >= 8:
            break
        zeros = zeros + 1 if n <= 0 else 0
        if zeros >= 36:
            raise RuntimeError(f"[{target}] still 0 items after 3 minutes — scan never started")
        last = n
        time.sleep(5)
    # Movie,Episode is the fair figure for the report (folders resolve differently per server).
    return item_count(base, ctx, "Movie,Episode")


def provision(base, target, ctx):
    """Add the libraries from the LIBRARIES env (real media and/or synthetic
    padding) and kick a scan."""
    h = token_headers(ctx["token"])
    # Fairness: BOTH servers get these options, never just one. Ferrofin's
    # remote providers used to be inert (feature-gated / no keys) so only
    # Jellyfin needed muzzling — that premise died when TMDB/TVDB/MusicBrainz/
    # AudioDb/fanart/Studio-Images shipped on-by-default with a built-in key.
    # Sending this to Jellyfin alone then meant: Ferrofin fetched remote
    # metadata for every item over the network while Jellyfin fetched none, so
    # Ferrofin carried richer DTOs (bigger bodies to serialize on every browse
    # row) AND ran a realtime filesystem watcher Jellyfin had switched off —
    # measuring two different workloads and flattering Jellyfin on both.
    # Empty fetcher lists gate only the NAMED remote fetchers (and embedded
    # image extraction) on either server; local NFO and on-disk images are
    # ungated in Ferrofin's scanner and unaffected in Jellyfin.
    no_remote = {
        "LibraryOptions": {
            "EnableRealtimeMonitor": False,
            "SaveLocalMetadata": False,
            "TypeOptions": [
                {"Type": t, "MetadataFetchers": [], "MetadataFetcherOrder": [],
                 "ImageFetchers": [], "ImageFetcherOrder": []}
                for t in ("Movie", "Series", "Season", "Episode",
                          "MusicArtist", "MusicAlbum", "Audio")
            ],
        },
    }
    # Idempotent by name: re-provisioning a server whose DB survived (a resumed
    # run, a kept volume) must NOT add the same library twice — both servers
    # happily accept the duplicate and then scan everything double, silently
    # changing the workload (observed live: item count ~2×).
    existing = {v.get("Name") for v in (get_json(f"{base}/Library/VirtualFolders", h) or [])}
    for lib in json.loads(os.environ.get("LIBRARIES", "[]")):
        if lib["name"] in existing:
            continue
        q = (f"name={urllib.parse.quote(lib['name'])}&collectionType={lib['type']}"
             f"&paths={urllib.parse.quote(lib['path'])}")
        # Always send a real JSON body: an empty body with a JSON content-type is a 400 on Ferrofin.
        refresh = "&refreshLibrary=true" if target == "jellyfin" else ""
        status, resp = request("POST", f"{base}/Library/VirtualFolders?{q}{refresh}", no_remote, h)
        if status >= 300:
            raise RuntimeError(f"[{target}] add library {lib['name']!r} failed: {status} {resp[:200]!r}")
    if target != "jellyfin":
        status, _ = request("POST", f"{base}/Library/Refresh", None, h)  # ferrofin: kick the scan
        if status >= 300:
            raise RuntimeError(f"[{target}] /Library/Refresh failed: {status}")


def jellyfin_first_run_wizard(base, target):
    """Jellyfin's first boot needs the startup wizard completed before
    AuthenticateByName works. /System/Info/Public 200s while migrations are
    still seeding, so retry until Complete sticks."""
    jh = {"Content-Type": "application/json"}
    for _ in range(60):
        status, _ = request(
            "POST", f"{base}/Startup/Configuration",
            {"UICulture": "en-US", "MetadataCountryCode": "US", "PreferredMetadataLanguage": "en"}, jh)
        if status < 300:
            request("GET", f"{base}/Startup/User")
            request("POST", f"{base}/Startup/User", {"Name": USER, "Password": PASS}, jh)
            done, _ = request("POST", f"{base}/Startup/Complete", None, jh)
            if done < 300:
                return
        time.sleep(2)
    raise RuntimeError(f"[{target}] startup wizard never completed")


def bring_up(base, target):
    """Provision one server end-to-end and return its ready ctx
    (token, userId, itemsFound, username/password for body templates)."""
    if target == "jellyfin":
        jellyfin_first_run_wizard(base, target)
    ctx = authenticate(base, target)
    ctx["username"], ctx["password"] = USER, PASS
    provision(base, target, ctx)
    ctx["itemsFound"] = wait_for_scan(base, target, ctx)
    return ctx


def pick_items(base, ctx):
    """Deterministic item pick: first movie by SortName on BOTH servers (ids
    differ, the item is the same). For the image rows, prefer the first with a
    discovered Primary image — a poster-less pick turns the row into a 404
    microbenchmark."""
    j = get_json(
        f"{base}/Items?userId={ctx['userId']}&Recursive=true&IncludeItemTypes=Movie"
        f"&SortBy=SortName&Limit=200", token_headers(ctx["token"])) or {}
    items = j.get("Items") or []
    ctx["itemId"] = items[0]["Id"] if items else ""
    with_image = next((i for i in items if (i.get("ImageTags") or {}).get("Primary")), None)
    ctx["imageItemId"] = with_image["Id"] if with_image else ctx["itemId"]
    return ctx


def first_name(base, ctx, route):
    """The first by-name facet entry (`/Genres`, `/Studios`, `/Persons`, …),
    URL-quoted for direct substitution into a path template.

    Both servers sort these routes by SortName ascending, so "first" names the
    same real-world entity on each even though the ids differ. Quoting happens
    here, once: person and studio names carry spaces and apostrophes ("'Weird
    Al' Yankovic"), and an unquoted one produces an invalid vegeta target
    rather than a measurement. Returns '' when the library has no such facet
    (the by-name detail row then 404s identically on both servers, matching the
    rest of enrich_context's missing-shape convention)."""
    j = get_json(f"{base}/{route}?userId={ctx['userId']}&limit=1", token_headers(ctx["token"])) or {}
    items = j.get("Items") or []
    return urllib.parse.quote(items[0].get("Name", "")) if items else ""


def find_playlist(items, name):
    """The id of the playlist called `name` in a `/Items` page, or '' — pure so
    enrich_context's reuse-don't-recreate rule is unit-testable."""
    return next((i.get("Id", "") for i in items if i.get("Name") == name), "")


def enrich_context(base, ctx):
    """Resolve the extra context ids the expanded endpoint set needs, after
    auth + scan: the first series (TV browse), a scheduled task id, the picked
    image item's cache tag, the first genre/studio/person name (by-name detail
    rows), and a bench playlist (reused if one already exists). Fields default
    to '' so a library without that shape yields 404s on both servers
    identically rather than breaking the run."""
    h = token_headers(ctx["token"])
    series = get_json(
        f"{base}/Items?userId={ctx['userId']}&recursive=true&includeItemTypes=Series"
        f"&sortBy=SortName&limit=1", h) or {}
    ctx["seriesId"] = (series.get("Items") or [{}])[0].get("Id", "") if series.get("Items") else ""
    tasks = get_json(f"{base}/ScheduledTasks", h) or []
    ctx["taskId"] = (tasks[0].get("Id") or tasks[0].get("Key", "")) if tasks else ""
    img = get_json(
        f"{base}/Items?userId={ctx['userId']}&recursive=true&includeItemTypes=Movie"
        f"&sortBy=SortName&limit=200", h) or {}
    movies = img.get("Items") or []
    with_image = next((i for i in movies if (i.get("ImageTags") or {}).get("Primary")), None)
    ctx["imageTag"] = with_image["ImageTags"]["Primary"] if with_image else "0"
    ctx["genreName"] = first_name(base, ctx, "Genres")
    ctx["studioName"] = first_name(base, ctx, "Studios")
    ctx["personName"] = first_name(base, ctx, "Persons")
    # Reuse the bench playlist if one is already there. Creating unconditionally
    # was fine for the suite's fresh-DB-per-run legs but not for the fast loop,
    # which points many ctx refreshes at ONE long-lived database: every refresh
    # added another "bench-playlist" item, so the Playlists folder — and the
    # totals the /Items rows measure — drifted upward run over run.
    existing = get_json(
        f"{base}/Items?userId={ctx['userId']}&recursive=true&includeItemTypes=Playlist"
        f"&limit=200", h) or {}
    ctx["playlistId"] = find_playlist(existing.get("Items") or [], PLAYLIST_NAME)
    if not ctx["playlistId"]:
        status, created = request(
            "POST", f"{base}/Playlists",
            {"Name": PLAYLIST_NAME, "UserId": ctx["userId"],
             "Ids": [ctx["itemId"]] if ctx.get("itemId") else []}, h)
        ctx["playlistId"] = (json.loads(created).get("Id", "") if status < 300 else "")
    # Write rows target the LAST movie by SortName — never ctx.itemId (the read
    # rows' item), so write traffic can't drift a read row's body/fingerprint.
    ctx["writeItemId"] = movies[-1]["Id"] if movies else ctx.get("itemId", "")
    # One playstate start so playstate_progress measures the steady-state
    # upsert, not a first-report session create. Best-effort like the playlist.
    request("POST", f"{base}/Sessions/Playing",
            {"ItemId": ctx["writeItemId"], "PositionTicks": 0, "CanSeek": True,
             "PlayMethod": "DirectPlay"}, h)
    return ctx
