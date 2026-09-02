#!/usr/bin/env python3
"""Layer-1 breadth sweep: contract-conformance across all 412 operations.

For every operation in the vendored Jellyfin spec, generate a request (path
params filled from a live fixture library, minimal query, empty/example body for
writes), send it to Ferrofin and — if a Jellyfin oracle URL is given — to Jellyfin,
and record two signals per op into `parity/sweep-results.json`:

  status_conformant  Ferrofin's HTTP status *class* (2xx/4xx/5xx) matches Jellyfin's.
                     null if no oracle, or the request couldn't be built.
  schema_valid       Ferrofin's 2xx JSON body validates against the op's response
                     schema in the vendored spec ($ref-resolved, OpenAPI-nullable
                     aware). null for empty/non-JSON/non-2xx responses.

`gen-ledger.py` ingests the results file (same as seed.json) — this script never
touches the ledger directly.

Provisioning (wizard/auth/libraries/scan-wait) is ported from benchmark/bench-lib.js;
sweep.sh brings the two servers up via docker and passes their URLs.

Run (both servers already up + LIBRARIES env set — see sweep.sh):
  FERROFIN_URL=http://localhost:18096 JELLYFIN_URL=http://localhost:18097 parity/sweep.py
Offline self-check (no servers):
  parity/sweep.py --check
"""
import glob
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import warnings

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
METHODS = ("get", "post", "put", "delete", "patch", "head")
USER = os.environ.get("BENCH_ADMIN_USER", "bench")
PASS = os.environ.get("BENCH_ADMIN_PASSWORD", "benchpass123")
# Modern MediaBrowser grammar only — 10.11 rejects X-Emby-* (see bench-lib.js).
CLIENT = 'Client="parity", Device="parity", DeviceId="parity", Version="1.0"'

# ---------------------------------------------------------------- docker (host-side effects)

def compose_service(base):
    """The docker-compose service behind a server URL: the parity leg runs both servers from
    suite/perf/docker-compose.yml, so FERROFIN_URL ↔ `ferrofin`, JELLYFIN_URL ↔ `jellyfin`
    (overridable via FERROFIN_SERVICE / JELLYFIN_SERVICE). None for an unknown URL."""
    if base == os.environ.get("FERROFIN_URL", "http://localhost:18096"):
        return os.environ.get("FERROFIN_SERVICE", "ferrofin")
    if base == os.environ.get("JELLYFIN_URL", "http://localhost:18097"):
        return os.environ.get("JELLYFIN_SERVICE", "jellyfin")
    return None


def compose(*args, timeout=60):
    """Run `docker compose <args>` against the suite's compose project (cwd suite/perf, so the
    project name / overrides come from the environment exactly as sweep.sh set them).
    Returns (returncode, stdout); returncode -1 when docker is unavailable."""
    import subprocess
    try:
        out = subprocess.run(["docker", "compose", *args], capture_output=True, text=True,
                             timeout=timeout, cwd=os.path.join(ROOT, "suite", "perf"))
        return out.returncode, out.stdout
    except (OSError, subprocess.TimeoutExpired):
        return -1, ""


def container_read(base, path):
    """Read a file from inside the container serving `base` (a host-side effect the HTTP
    surface only references, e.g. the password-reset PIN file). None when unreachable."""
    svc = compose_service(base)
    if not svc:
        return None
    rc, out = compose("exec", "-T", svc, "cat", path)
    return out if rc == 0 else None

# ---------------------------------------------------------------- HTTP

def http(method, url, token=None, body=None):
    headers = {"Content-Type": "application/json"}
    if token is not None:
        headers["Authorization"] = f'MediaBrowser Token="{token}", {CLIENT}'
    elif "AuthenticateByName" in url or "/Startup/" in url:
        headers["Authorization"] = f"MediaBrowser {CLIENT}"
    data = body.encode() if isinstance(body, str) else body
    req = urllib.request.Request(url, data=data, method=method.upper(), headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
            return r.status, raw
    except urllib.error.HTTPError as e:
        return e.code, e.read()
    except (urllib.error.URLError, TimeoutError, ConnectionError) as e:
        return 0, str(e).encode()


def get_json(base, path, token):
    st, raw = http("GET", base + path, token)
    if st == 200:
        try:
            return json.loads(raw)
        except ValueError:
            return None
    return None

# ---------------------------------------------------------------- provisioning (port of bench-lib.js)

def wizard(base):
    """Jellyfin first-boot: /Startup/Configuration → User → Complete (retry)."""
    cfg = json.dumps({"UICulture": "en-US", "MetadataCountryCode": "US", "PreferredMetadataLanguage": "en"})
    for _ in range(60):
        st, _ = http("POST", base + "/Startup/Configuration", body=cfg)
        if st < 300:
            http("GET", base + "/Startup/User")
            http("POST", base + "/Startup/User", body=json.dumps({"Name": USER, "Password": PASS}))
            done, _ = http("POST", base + "/Startup/Complete", body="")
            if done < 300:
                return
        time.sleep(2)
    raise SystemExit(f"{base}: startup wizard never completed")


def authenticate(base):
    st, raw = http("POST", base + "/Users/AuthenticateByName",
                   body=json.dumps({"Username": USER, "Pw": PASS}))
    if st != 200:
        raise SystemExit(f"{base}: auth failed {st}: {raw[:200]!r}")
    b = json.loads(raw)
    return b["AccessToken"], b["User"]["Id"]


def provision(base, target, token):
    # The opt-in OpenSubtitles plugin install restarts Jellyfin: it must happen BEFORE the
    # libraries are added, or the initial scan (which has no startup trigger to resume it)
    # is cut short and Jellyfin settles on a partial library.
    provision_opensubtitles(base, target, token)
    # Send BOTH servers the realistic jellyfin-web body shape: TypeOptions entries that OMIT
    # ImageOptions (and other arrays). This disables remote fetchers for fairness AND exercises the
    # exact deserialization path real clients use — a server missing serde container defaults 422s
    # here and fails the sweep loudly, instead of being masked by a minimal `{}` body.
    no_remote = {"LibraryOptions": {"EnableRealtimeMonitor": False, "SaveLocalMetadata": False,
        "TypeOptions": [{"Type": t, "MetadataFetchers": [], "MetadataFetcherOrder": [],
                         "ImageFetchers": [], "ImageFetcherOrder": []}
                        for t in ("Movie", "Series", "Season", "Episode",
                                  "MusicArtist", "MusicAlbum", "Audio")]}}
    for lib in json.loads(os.environ.get("LIBRARIES", "[]")):
        q = (f"name={urllib.parse.quote(lib['name'])}&collectionType={lib['type']}"
             f"&paths={urllib.parse.quote(lib['path'])}")
        if target == "jellyfin":
            q += "&refreshLibrary=true"
        st, raw = http("POST", f"{base}/Library/VirtualFolders?{q}", token, json.dumps(no_remote))
        if st >= 300:
            raise SystemExit(f"{target}: add library {lib['name']} failed {st}: {raw[:200]!r}")
    if target != "jellyfin":
        http("POST", base + "/Library/Refresh", token, None)


# Jellyfin's OpenSubtitles plugin (official repository) and Ferrofin's compiled-in provider.
OPENSUBTITLES_JELLYFIN_GUID = "4b9ed42f-5185-48b5-9803-6ff2989014c4"
OPENSUBTITLES_FERROFIN_GUID = "4a3f8e21-6c94-4d17-a2b8-0f5e9c3d7a10"


def opensubtitles_credentials():
    """(username, password, api_key) from the environment, or None — the remote-subtitle
    ops need a real opensubtitles.com account and the public internet, so they are opt-in:
    OPENSUBTITLES_USERNAME / OPENSUBTITLES_PASSWORD / OPENSUBTITLES_API_KEY."""
    u, p, k = (os.environ.get("OPENSUBTITLES_USERNAME"), os.environ.get("OPENSUBTITLES_PASSWORD"),
               os.environ.get("OPENSUBTITLES_API_KEY"))
    return (u, p, k) if u and p and k else None


# How long a restart (Jellyfin replays plugin/DB work on boot) may take; shared with
# terminal.py so a slow host raises one knob.
UP_TIMEOUT_S = int(os.environ.get("PARITY_UP_TIMEOUT_S", "300"))


POLL_S = 0.02   # fast enough to catch Ferrofin's sub-second in-process restart gap


def api_alive(base):
    """Reachable AND the real API is up. While Jellyfin re-boots in-process it runs a setup
    server on the same port that answers /System/Info/Public with JSON — but with
    StartupWizardCompleted=false — so a bare JSON 200 is not enough; both real servers
    report true here. (A plain-text 503 "Server is loading" follows for a moment after.)"""
    st, raw = http("GET", base + "/System/Info/Public")
    if st != 200 or not raw.lstrip().startswith(b"{"):
        return False
    try:
        return json.loads(raw).get("StartupWizardCompleted") is True
    except ValueError:
        return False


def wait_until(base, up, timeout_s):
    """True when the server reaches the wanted liveness within timeout_s."""
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        if api_alive(base) == up:
            return True
        time.sleep(POLL_S)
    return False


def wait_bounce(base, down_timeout_s=60):
    """After a restart request: the server goes unreachable, then reachable again."""
    return wait_until(base, False, down_timeout_s) and wait_until(base, True, UP_TIMEOUT_S)


def post_until_ready(base, path, token, body):
    """POST, retrying while the server is still coming up (connection refused → 0, or the
    plain-text 503 "Server is loading"). Returns the final status (0 = never reachable)."""
    deadline = time.time() + UP_TIMEOUT_S
    st = 0
    while time.time() < deadline:
        st, _ = http("POST", base + path, token, body)
        if st not in (0, 503):
            return st
        time.sleep(2)
    return st


def provision_opensubtitles(base, target, token):
    """With credentials in the environment: give both servers an OpenSubtitles provider.
    Ferrofin's is compiled in (configure {ApiKey, Username, Password}); Jellyfin's is the
    official plugin — installed from its repository, activated by an in-process restart,
    then configured with {Username, Password} (the plugin ships its own API key, so quota
    and result differences against Ferrofin's user key are possible)."""
    creds = opensubtitles_credentials()
    if not creds:
        return
    username, password, api_key = creds
    if target == "jellyfin":
        packages = get_json(base, "/Packages", token) or []
        pkg = next((p for p in packages if p.get("guid", "").lower() == OPENSUBTITLES_JELLYFIN_GUID
                    or (p.get("name") or "").lower() == "open subtitles"), None)
        if not pkg:
            raise SystemExit(f"{base}: the Open Subtitles package is not in the configured repositories")
        st, raw = http("POST", f"{base}/Packages/Installed/{urllib.parse.quote(pkg['name'])}"
                               f"?assemblyGuid={OPENSUBTITLES_JELLYFIN_GUID}", token, "")
        # 0 here is the 30 s client timeout on a slow repository: the server-side install
        # carries on, the /Packages/Installing poll below covers it, and a plugin that never
        # lands surfaces as a 404 on the configuration POST — so only a real error status fails.
        if st >= 300:
            raise SystemExit(f"{base}: plugin install failed {st}: {raw[:200]!r}")
        for _ in range(120):   # download + stage
            if not (get_json(base, "/Packages/Installing", token) or []):
                break
            time.sleep(2)
        http("POST", base + "/System/Restart", token, "")
        if not wait_bounce(base):
            raise SystemExit(f"{base}: did not bounce after the plugin-activating restart")
        st = post_until_ready(base, f"/Plugins/{OPENSUBTITLES_JELLYFIN_GUID}/Configuration", token,
                              json.dumps({"Username": username, "Password": password}))
    else:
        st = post_until_ready(base, f"/Plugins/{OPENSUBTITLES_FERROFIN_GUID}/Configuration", token,
                              json.dumps({"Username": username, "Password": password, "ApiKey": api_key}))
    if st == 0 or st >= 300:
        raise SystemExit(f"{target}: opensubtitles configuration failed (status {st})")


#: The fake HDHomeRun device the `hdhomerun-source` compose service runs. Both servers get
#: a `hdhomerun` tuner host pointed here, so the SECOND tuner backend is exercised — and
#: diffed — against one real device interface rather than against nothing.
#:
#: Overridable so a run against a PHYSICAL HDHomeRun can point at it instead; unset it to
#: provision the M3U tuner alone.
LIVETV_HDHR = os.environ.get("LIVETV_HDHR", "http://hdhomerun-source:8100")


def provision_livetv(base, token):
    """The Live TV fixture: an M3U tuner host, an HDHomeRun tuner host (the fake device on
    the compose network) and one XMLTV listings provider, then the guide refresh task,
    waited on until channels and programmes are listed. No-op when the fixture is off
    (LIVETV_M3U unset)."""
    m3u, xmltv = os.environ.get("LIVETV_M3U"), os.environ.get("LIVETV_XMLTV")
    if not m3u or not xmltv:
        return
    st, raw = http("POST", base + "/LiveTv/TunerHosts", token,
                   json.dumps({"Type": "m3u", "Url": m3u, "FriendlyName": "Parity tuner",
                               "ImportFavoritesOnly": False, "AllowHWTranscoding": False}))
    if st >= 300:
        raise SystemExit(f"{base}: add tuner host failed {st}: {raw[:200]!r}")
    if LIVETV_HDHR:
        # `TunerHostManager.SaveTunerHost` runs the host's `Validate` before storing, so a
        # non-2xx here means the device did not answer discover.json on ONE of the servers
        # — which is a finding, not something to skip past.
        st, raw = http("POST", base + "/LiveTv/TunerHosts", token,
                       # `AllowHWTranscoding` is what opens
                       # `GetChannelStreamMediaSources`' six-profile fan-out
                       # (HdHomerunHost.cs:339-379) on a device whose
                       # ModelNumber contains "hdtc" — the fake is an EXTEND, so
                       # with it on BOTH servers emit heavy / internet540 /
                       # internet480 / internet360 / internet240 / mobile /
                       # native and every `GetMediaSource` arm is diffed. Off,
                       # only `native` is, which is one arm of six.
                       json.dumps({"Type": "hdhomerun", "Url": LIVETV_HDHR,
                                   "ImportFavoritesOnly": False, "AllowHWTranscoding": True}))
        if st >= 300:
            raise SystemExit(f"{base}: add hdhomerun tuner host failed {st}: {raw[:200]!r}")
    st, raw = http("POST", base + "/LiveTv/ListingProviders?validateListings=false", token,
                   json.dumps({"Type": "xmltv", "Path": xmltv, "EnableAllTuners": True}))
    if st >= 300:
        raise SystemExit(f"{base}: add listings provider failed {st}: {raw[:200]!r}")
    tasks = get_json(base, "/ScheduledTasks", token) or []
    guide = next((t for t in tasks if t.get("Key") == "RefreshGuide"), None)
    if guide:
        http("POST", f"{base}/ScheduledTasks/Running/{guide['Id']}", token, "")
    for _ in range(120):
        channels = get_json(base, f"/LiveTv/Channels?userId={CTX_USER[base]}", token) or {}
        ids = [c["Id"] for c in channels.get("Items") or []]
        if ids:
            programs = get_json(base, f"/LiveTv/Programs?channelIds={ids[0]}&isAiring=true"
                                      f"&userId={CTX_USER[base]}", token) or {}
            if programs.get("Items"):
                return
        time.sleep(5)
    raise SystemExit(f"{base}: live tv channels/programmes never appeared")


def wait_for_scan(base, token):
    """Until the library scan has settled: the Movie+Episode count is stable for 40 s and
    non-zero. Counted by type on purpose — Live TV channels and music tracks are items too,
    and a couple of them appearing first must not read as "the scan is done"."""
    def count():
        b = get_json(base, "/Items?userId=%s&recursive=true&includeItemTypes=Movie,Episode&limit=0"
                     % CTX_USER[base], token)
        return b.get("TotalRecordCount", 0) if b else -1
    last, stable, zeros = -1, 0, 0
    for _ in range(480):
        n = count()
        stable = stable + 1 if (n == last and n > 0) else 0
        if stable >= 8:
            return
        # A scan that never produces an item is not a slow scan, it is a broken
        # provision, and the 40-minute cap below used to absorb it SILENTLY: the
        # loop fell through and every layer then measured an empty library, so
        # the run reported weaker numbers instead of failing. Seen 2026-08-31,
        # when Jellyfin scanned nothing and the abort surfaced four layers later
        # as "need >=2 movies, got 0". Three minutes of zeros is already
        # pathological -- the healthy pair reaches hundreds of items in seconds.
        zeros = zeros + 1 if n <= 0 else 0
        if zeros >= 36:
            raise RuntimeError(
                "%s: still 0 Movie/Episode items after 3 minutes -- the scan never "
                "started. Refusing to measure an empty library." % base)
        last = n
        time.sleep(5)
    raise RuntimeError(
        "%s: library scan never settled (last count %d). Refusing to measure a "
        "half-scanned library -- every layer after this would report a number "
        "that looks like a regression and is not one." % (base, last))


CTX_USER = {}

def ensure_livetv(base, token):
    """Provision the Live TV fixture exactly once. provision_livetv's tuner
    POSTs are NOT idempotent (each call adds another tuner host), and the
    already-provisioned early return in bring_up can run more than once per
    docker cycle — channels>0 is the already-done signal (provision_livetv
    itself waits until channels are listed before returning)."""
    if not os.environ.get("LIVETV_M3U"):
        return
    b = get_json(base, "/LiveTv/Channels?limit=0", token)
    if b is None:
        # Fail LOUD, not open: treating a probe failure as "no channels" would
        # re-run the non-idempotent tuner POSTs and duplicate the tuner host.
        raise SystemExit(f"{base}: /LiveTv/Channels probe failed — cannot decide livetv provisioning")
    if b.get("TotalRecordCount", 0) > 0:
        return
    provision_livetv(base, token)


def bring_up(base, target):
    # Idempotent: if already provisioned (e.g. an earlier producer in the same docker cycle),
    # just connect — don't re-run the wizard (fails once setup is complete) or re-add libraries.
    provisioned = False
    try:
        token, user = authenticate(base)
        CTX_USER[base] = user
        b = get_json(base, f"/Items?userId={user}&recursive=true&limit=0", token)
        provisioned = bool(b) and b.get("TotalRecordCount", 0) > 0
    except SystemExit:
        pass
    if provisioned:
        # Testdata mode always lands here (the seeded snapshot has items), so
        # the Live TV fixture must be provisioned on THIS path or the whole
        # LiveTv row family silently degrades to empty. OUTSIDE the try above:
        # ensure_livetv fails loud via SystemExit, which the except would
        # swallow and re-diagnose as a seeding problem.
        if os.environ.get("BENCH_TESTDATA") == "1":
            ensure_livetv(base, token)
        return token, user
    # Testdata mode NEVER provisions: reaching here means the seeded snapshot
    # is not being served, and falling into wizard()+provision() against an
    # empty server would fail minutes later pointing away from the real cause.
    if os.environ.get("BENCH_TESTDATA") == "1":
        raise SystemExit(f"{base}: testdata mode: server has no items — was the config volume seeded before start?")
    if target == "jellyfin":
        wizard(base)
    token, user = authenticate(base)
    CTX_USER[base] = user
    provision(base, target, token)
    wait_for_scan(base, token)
    # After the scan: the tuner/guide refresh must not compete with it, and its channel
    # items must not be mistaken for scan progress.
    provision_livetv(base, token)
    return token, user

# ---------------------------------------------------------------- fixtures + request generation

def resolve_fixtures(base, token, user):
    """Live values for the common path params (see the param-frequency table)."""
    def first(kinds):
        b = get_json(base, f"/Items?userId={user}&recursive=true&includeItemTypes={kinds}"
                           f"&limit=1&sortBy=SortName", token)
        it = (b or {}).get("Items") or []
        return it[0]["Id"] if it else None
    movie = first("Movie") or first("Video")
    series = first("Series")
    season = first("Season")
    episode = first("Episode")
    audio = first("Audio")
    album = first("MusicAlbum")
    # NOT /Items?includeItemTypes=MusicArtist: Ferrofin serves artists as
    # parentless by-name rows, so that query is empty on one server and not the
    # other. /Artists returns all three on both.
    artists = get_json(base, f"/Artists?userId={user}&limit=1", token) or {}
    artist = ((artists.get("Items") or [{}])[0]).get("Id")
    any_item = movie or series or episode
    genres = get_json(base, f"/Genres?userId={user}&limit=1", token) or {}
    genre = (genres.get("Items") or [{}])[0].get("Name") or "Action"
    # A MUSIC genre for the /MusicGenres/* routes (see `music_genre_fixtures`).
    music_genres = get_json(base, f"/MusicGenres?userId={user}&limit=1&sortBy=SortName", token) or {}
    music_genre = (music_genres.get("Items") or [{}])[0].get("Name")
    sessions = get_json(base, "/Sessions", token) or []
    session = sessions[0]["Id"] if sessions else None
    logs = get_json(base, "/System/Logs", token) or []
    log_name = logs[0]["Name"] if logs and logs[0].get("Name") else None
    # `GET /LiveTv/Channels/{channelId}` and `/LiveTv/Channels/{channelId}/...`
    # were reported "unresolved path param" — the sweep never had a channel id to
    # substitute, so a route with a real implementation behind it went unprobed.
    # The Live TV fixture provisions the tuner, so the lineup is right there.
    channels = get_json(base, f"/LiveTv/Channels?userId={user}&limit=1", token) or {}
    channel = ((channels.get("Items") or [{}])[0]).get("Id")
    # `{pluginId}`/`{version}` were reported "unresolved path param" on five
    # plugin ops. The seed un-skips the TWO the breadth sweep actually fires —
    # `GET /Plugins/{id}/Configuration` and `GET /Plugins/{id}/{version}/Image`.
    # The other three (POST Configuration, POST Manifest, DELETE
    # {id}/{version}) are non-GET and are stamped "write: not fired by the
    # breadth sweep" further down regardless of the seed, which is the SAFE
    # behaviour and not an oversight: a fired DELETE would uninstall a bundled
    # plugin on the shared Jellyfin container. Their Layer-2 probes in reads.py
    # own them.
    #
    # Each side is seeded from its OWN `GET /Plugins`, which is the same thing
    # the `groupId` seed does above and for the same reason: the value only has
    # to be a real one on the server being asked, and what the breadth row then
    # measures is status parity — that both servers answer the same way for a
    # plugin id that exists on them.
    #
    # The 200 BODY on these rows is out of reach by construction and must not be
    # claimed: the two servers share no plugin id (Ferrofin ships compiled-in
    # extensions and WASM components, stock Jellyfin ships five bundled .NET
    # provider plugins), so no shared subject exists to body-diff. reads.py's
    # `plugin_invariants` is what earns the verification, by holding each
    # server's OWN answer to the same shape.
    plugins = get_json(base, "/Plugins", token) or []
    plugin = plugins[0].get("Id") if plugins else None
    plugin_version = plugins[0].get("Version") if plugins else None

    def source_id(item_id):
        """The item's media source id from PlaybackInfo — what a client sends as
        mediaSourceId (Ferrofin's is not the item id's hyphenated spelling)."""
        if not item_id:
            return None
        info = get_json(base, f"/Items/{item_id}/PlaybackInfo?userId={user}", token) or {}
        sources = info.get("MediaSources") or []
        return (sources[0].get("Id") or item_id) if sources else item_id

    movie_src = source_id(movie or any_item)
    fx = {
        "itemId": any_item, "videoId": movie or any_item, "id": any_item, "Id": any_item,
        "routeItemId": any_item, "mediaSourceId": movie_src, "routeMediaSourceId": movie_src,
        "seriesId": series or any_item, "SeriesId": series or any_item,
        "SeasonId": season or any_item, "userId": user, "sessionId": session,
        "name": genre, "genreName": genre, "imageType": "Primary",
        "imageIndex": "0", "index": "0", "newIndex": "0", "routeIndex": "0",
        # `channelId` is TWO different id spaces in this contract: a Live TV
        # channel under /LiveTv/, and a plugin-channel under /Channels/. It is
        # scoped by path (see PATH_SCOPED) rather than put in `fx`, so filling
        # one cannot silently probe the other with a wrong id.
        "_livetv_channel": channel,
        # The /Channels/ half of that split — same argument as `groupId` below.
        # `ChannelsController` serves `IChannel` PROVIDER channels, and neither
        # the v10.11.8 tree nor master contains a single `IChannel`
        # implementation (`git grep -l "IChannel" -- "*.cs"` returns only the
        # interface and its three consumers on both), so no `Channel` item can
        # exist on either server and the response CANNOT depend on the value. A
        # literal GUID is therefore the only possible seed and the right one:
        # the breadth row then measures the 400/400 status parity
        # (`GetChannel(id)` is null -> `GetChannelProvider(null)` ->
        # `ArgumentNullException.ThrowIfNull` -> `ExceptionMiddleware`
        # `ArgumentException => 400`) instead of skipping the op. It stays a
        # NON-deep row here — sweep body-diffs only 200/200 — which is correct:
        # there is no 200 body on either server, ever. Leaving it unseeded is
        # what hid a live Ferrofin bug (a fabricated 200 `ChannelFeatures`
        # echoing the requested id) behind a "requires-channel-plugin" label.
        "_plugin_channel": "11111111-1111-1111-1111-111111111111",
        # `{groupId}` occurs in exactly ONE contract path, and
        # `LiveTvController.GetRecordingGroup` (v10.11.8) is `[Obsolete]` with
        # the body `return NotFound();` — no `RecordingGroup` lookup exists
        # anywhere in the 10.11.8 tree, so the response cannot depend on the
        # value. A literal GUID is therefore the only possible seed AND the
        # right one: the breadth row then measures the 404/404 status parity
        # instead of skipping the op. It stays a NON-deep row here (sweep
        # body-diffs only 200/200); reads.py's `recording_group_invariants`
        # is what earns the verification.
        "groupId": "00000000-0000-0000-0000-000000000000",
        "year": "2020", "container": "mp4", "segmentContainer": "ts", "format": "ts",
        "routeFormat": "ts", "width": "400", "maxWidth": "400", "maxHeight": "400",
        "percentPlayed": "0", "unplayedCount": "0", "tag": "x", "language": "eng",
        "routeStartPositionTicks": "0", "streamId": "0", "logName": log_name,
        # Not a path param: the first track, so the /Audio/* ops probe a real audio item
        # (see `audio_fixtures`).
        "_audio": audio, "_audio_src": source_id(audio),
        # Kind-correct seeds for the /Similar aliases (see `similar_fixtures`).
        "_album": album, "_artist": artist, "_series": series,
        # This server's own first package name, for `/Packages/{name}` (see
        # `package_fixtures`). The generic `{name}` fill is a GENRE, so without
        # this the probe was literally `GET /Packages/Action` — 404 on both
        # servers, scored status-conformant, and the lookup never exercised.
        "_package": first_package_name(base, token),
        "pluginId": plugin,
        # `{version}` occurs only under `/Plugins/`, and it must be the
        # INSTALLED one: `PluginManager.GetPlugin(id, version)` matches with
        # `Version.Equals` (v10.11.8 Emby.Server.Implementations/Plugins/
        # PluginManager.cs:293-311), so any other string is a 404 and the row
        # would measure the miss path instead of the hit path.
        "_plugin_version": plugin_version,
        # The music-genre seed for the /MusicGenres/* routes (see `music_genre_fixtures`).
        "_musicgenre": music_genre,
    }
    return {k: v for k, v in fx.items() if v is not None}


def audio_fixtures(fixtures):
    """The fixtures with the item/source ids swapped for the first audio track — what the
    /Audio/* ops are about. Unchanged when the fixture has no music library."""
    audio = fixtures.get("_audio")
    if not audio:
        return fixtures
    return {**fixtures, "itemId": audio, "mediaSourceId": fixtures.get("_audio_src") or audio}


# The `{itemId}` seed each /{kind}/{itemId}/Similar alias is ABOUT. Without this
# every alias was probed with `any_item` (a Movie), so /Albums, /Artists and
# /Shows Similar were byte-identical copies of the /Movies row and proved
# nothing about their own controller path — they never touched an album, an
# artist or a series seed at all.
# /Items, /Movies and /Trailers are NOT listed on purpose: `LibraryController
# .GetSimilarItems` dispatches on the SEED's kind, never on the route, so all
# three want the movie seed `any_item` already carries — and the loop fixture
# has no Trailer item to seed /Trailers with anyway. (/Trailers' own 404,
# nil-seed and count contract is issued against /Trailers by the reads.py
# `similar_invariants_trailers` probe, not here.)
SIMILAR_SEEDS = {
    "/Albums/{itemId}/Similar": "_album",
    "/Artists/{itemId}/Similar": "_artist",
    "/Shows/{itemId}/Similar": "_series",
}


def first_package_name(base, token):
    """The first entry of this server's own plugin catalogue, or None.

    `GET /Packages` is admin-only and repository-dependent; a server with no
    reachable repository has an empty catalogue, and then `/Packages/{name}` has
    nothing to resolve and the sweep skips it with a reason (which is honest)
    rather than probing an unrelated string."""
    body = get_json(base, "/Packages", token)
    if isinstance(body, list) and body:
        return body[0].get("name")
    return None


# Paths whose `{name}` is NOT the generic by-name (genre) seed. `/Packages/{name}`
# names a PACKAGE; filling it with a genre made the row a permanent 404/404.
NAME_SEEDS = {"/Packages/{name}": "_package"}


def package_fixtures(path, fixtures):
    """The fixtures with `name` swapped for the path-appropriate seed.

    Drops `name` entirely when the server has no such seed, so `build_url`
    reports "unresolved path param: name" instead of probing a wrong one."""
    key = NAME_SEEDS.get(path)
    if key is None:
        return fixtures
    seed = fixtures.get(key)
    out = {**fixtures}
    if seed:
        out["name"] = seed
    else:
        out.pop("name", None)
    return out
def music_genre_fixtures(path, fixtures):
    """The fixtures with the by-name seed swapped for a real MUSIC genre on the
    /MusicGenres/* routes.

    Both `{genreName}` and `{name}` were seeded from `/Genres` — a MOVIE genre
    — which made this sweep a WRITE against Jellyfin: `GetMusicGenre` is
    `CreateItemByName<MusicGenre>`, so probing `/MusicGenres/Action`
    materializes a `MusicGenre` row named after a movie genre, permanently, on
    every run. That row then showed up in Jellyfin's `/MusicGenres` listing and
    nowhere else, turning the `GET /MusicGenres` row red for the rest of the
    campaign. Seeding from `/MusicGenres` probes the route with something it is
    actually about and leaves no residue. Unchanged when the fixture has no
    music library."""
    seed = fixtures.get("_musicgenre")
    if not seed or not path.startswith("/MusicGenres/"):
        return fixtures
    return {**fixtures, "genreName": seed, "name": seed}


def similar_fixtures(path, fixtures):
    """The fixtures with `itemId` swapped for the kind-correct seed of a /Similar
    alias. Unchanged for every other path, and when the fixture lacks that kind."""
    seed = fixtures.get(SIMILAR_SEEDS.get(path, ""))
    return {**fixtures, "itemId": seed} if seed else fixtures


# REQUIRED query params the breadth sweep can fill from the shared fixture — the query-side
# counterpart of resolve_fixtures(): the item's own media source, a subtitle segment length,
# the shared media mount (identical in both containers), the first log file. A required param
# NOT listed here stays unfilled: the op then 400s on both and says so. (`path` also reaches
# GET /Backup/Manifest, where it names an archive: both 404 on the media dir — status parity.)
QUERY_FILL = {
    "mediaSourceId": lambda fx: fx.get("mediaSourceId"),
    "segmentLength": lambda fx: "10",
    "path": lambda fx: "/media/synth/movies",
    "name": lambda fx: fx.get("logName"),
}


# Path params whose meaning depends on where they appear. `(path prefix, param)
# -> fixture key`: only a path under the prefix gets the value, so a name shared
# by two unrelated id spaces cannot cross-contaminate.
PATH_SCOPED = {("/LiveTv/", "channelId"): "_livetv_channel",
               ("/Channels/", "channelId"): "_plugin_channel",
               ("/Plugins/", "version"): "_plugin_version"}


def scoped_fixtures(path, fixtures):
    """`fixtures` plus the path-scoped entries that apply to `path`."""
    extra = {param: fixtures[key]
             for (prefix, param), key in PATH_SCOPED.items()
             if path.startswith(prefix) and fixtures.get(key)}
    return {**fixtures, **extra} if extra else fixtures


def build_url(path, fixtures):
    """Fill path params from one server's fixtures. Return (url, skip_reason_or_None)."""
    fixtures = scoped_fixtures(path, fixtures)
    params = set(re.findall(r"{(\w+)}", path))
    missing = [p for p in params if p not in fixtures]
    if missing:
        return None, "unresolved path param: " + ",".join(sorted(missing))
    url = path
    for p in params:
        url = url.replace("{%s}" % p, urllib.parse.quote(str(fixtures[p]), safe=""))
    return url, None


def with_user_query(url, op, params, user, fixtures=None):
    """Inject the query params a breadth probe needs: userId when the op declares one (and it
    isn't in the path), every REQUIRED query param QUERY_FILL can supply, and `static=true` on
    ops that declare it — direct play on the /stream ops, copy codecs on the HLS playlists, so
    Layer-1 never spawns a transcode; the transcode path is streams.py's job."""
    fixtures = fixtures or {}
    qp = {pp.get("name"): pp for pp in op.get("parameters", []) if pp.get("in") == "query"}
    add = []
    if "userId" in qp and "userId" not in params:
        add.append(("userId", user))
    for name, pp in qp.items():
        if pp.get("required") and name in QUERY_FILL and name not in params:
            v = QUERY_FILL[name](fixtures)
            if v is not None:
                add.append((name, str(v)))
    if "static" in qp:
        add.append(("static", "true"))
    if add:
        url += ("&" if "?" in url else "?") + urllib.parse.urlencode(add)
    return url

# ---------------------------------------------------------------- schema validation

def make_validator(spec):
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        from jsonschema import Draft7Validator, validators, RefResolver

        def _type(validator, types, instance, schema):
            if instance is None and schema.get("nullable"):   # OpenAPI nullable, not JSON-Schema
                return
            yield from Draft7Validator.VALIDATORS["type"](validator, types, instance, schema)

        Nullable = validators.extend(Draft7Validator, {"type": _type})
        resolver = RefResolver.from_schema(spec)

        def validate(schema, instance):
            try:
                Nullable(schema, resolver=resolver).validate(instance)
                return True
            except Exception:
                return False
        return validate


def response_schema(op):
    resp = op.get("responses", {})
    key = "200" if "200" in resp else next((k for k in resp if k.startswith("2")), None)
    if not key:
        return None
    return (resp[key].get("content", {}).get("application/json", {}) or {}).get("schema")

# ---------------------------------------------------------------- deep diff (Layer-2 over all GET ops)

from parity_diff import diff_stats  # noqa: E402
import samples  # noqa: E402
import verification  # noqa: E402


def dedup_fields(buckets):
    """Collapse per-item [key] prefixes so each divergent FIELD is listed once."""
    def fields(bucket):
        return sorted({re.sub(r"^\[[^\]]*\]\.?", "", m["path"]) for m in bucket})
    return {k: fields(buckets[k]) for k in ("missing", "extra", "mismatch")}


# ---------------------------------------------------------------- sweep

def load_spec():
    f = sorted(glob.glob(os.path.join(ROOT, "contracts/jellyfin-openapi-*.json")))[-1]
    return json.load(open(f))


def sweep(ferrofin_url, jellyfin_url):
    spec = load_spec()
    validate = make_validator(spec)
    ht, hu = bring_up(ferrofin_url, "ferrofin")
    fixtures = resolve_fixtures(ferrofin_url, ht, hu)
    if jellyfin_url:
        jt, ju = bring_up(jellyfin_url, "jellyfin")

    fixtures_j = resolve_fixtures(jellyfin_url, jt, ju) if jellyfin_url else {}

    results = {}
    for path, item in spec["paths"].items():
        params = set(re.findall(r"{(\w+)}", path))
        for method, op in item.items():
            if method not in METHODS:
                continue
            opkey = f"{method.upper()} {path}"
            # Non-GET ops are ordered and often destructive, so the breadth sweep
            # does not fire them; a Layer-2 probe owns each one. Which probe is
            # the OP's business, not this stamp's: the RemoteSearch/<Kind> family
            # is a POST-shaped SEARCH and lives in reads.py, everything that
            # mutates lives in journeys.py. (The note used to say "deferred to
            # Layer-2 journey", which named the wrong layer for those rows and
            # asserted a state this project does not have — see CLAUDE.md.)
            if method not in ("get", "head"):
                results[opkey] = {"status_conformant": None, "schema_valid": None,
                                  "note": "write: not fired by the breadth sweep — "
                                          "owned by a Layer-2 probe"}
                continue
            fx_h = audio_fixtures(fixtures) if path.startswith("/Audio/") else fixtures
            fx_j = audio_fixtures(fixtures_j) if path.startswith("/Audio/") else fixtures_j
            fx_h = package_fixtures(path, similar_fixtures(path, fx_h))
            fx_j = package_fixtures(path, similar_fixtures(path, fx_j))
            fx_h = music_genre_fixtures(path, fx_h)
            fx_j = music_genre_fixtures(path, fx_j)
            hurl, skip = build_url(path, fx_h)   # per-server ids: Ferrofin's on Ferrofin
            if skip:
                results[opkey] = {"status_conformant": None, "schema_valid": None, "note": skip}
                continue
            hs, hraw = http(method, ferrofin_url + with_user_query(hurl, op, params, hu, fx_h), ht)
            # schema_valid: Ferrofin 2xx JSON vs response schema (needs no oracle)
            sv = None
            sch = response_schema(op)
            if 200 <= hs < 300 and sch is not None and hraw:
                try:
                    sv = validate(sch, json.loads(hraw))
                except ValueError:
                    sv = False
            if jellyfin_url:
                jurl, jskip = build_url(path, fx_j)   # Jellyfin's own ids on Jellyfin
                if jskip:
                    results[opkey] = {"status_conformant": None, "schema_valid": sv, "note": f"H={hs} J=n/a"}
                    continue
                js, jraw = http(method, jellyfin_url + with_user_query(jurl, op, params, ju, fx_j), jt)
                row = {"status_conformant": (hs // 100) == (js // 100),
                       "schema_valid": sv, "note": f"H={hs} J={js}"}
                # Layer-2 deep diff over the whole GET surface: when BOTH return 200 JSON, diff the
                # bodies (Path/Name array alignment + volatile denylist). Single-item ops align
                # because "first item by SortName" is the same title on both servers; the curated
                # multi-item reads.py wins precedence in the ledger where it also covers an op.
                if method == "get" and hs == 200 and js == 200 and hraw and jraw:
                    try:
                        jb, hb = json.loads(jraw), json.loads(hraw)
                        n, buckets, compared = diff_stats(jb, hb)
                        # Keep the evidence. A verdict whose two bodies were
                        # discarded cannot be audited, and a wrong one looks
                        # exactly like a right one.
                        samples.record(opkey, jb, hb, route=f"{method.upper()} {op}",
                                       diff=buckets if n else None)
                        # HOW this row was verified, derived from what the diff
                        # actually walked — never assumed. `compared == 0` means the
                        # bodies were `[]`/`{}`/all-volatile and NOTHING was
                        # compared, which is untested, not verified; two empty
                        # result envelopes compared only their own zeros, which is
                        # `empty-corpus`, not the body-diff headline.
                        vm = verification.read_method(jb, hb, compared)
                        if n:
                            row["deep_verified"] = False
                            row["verification_method"] = vm or verification.BODY_DIFF
                            row["classification"] = "flagged: read diff vs Jellyfin (sweep single-item align)"
                            row["diffs"] = dedup_fields(buckets)
                        elif vm is None:
                            row["note"] += " | no comparable fields (nothing diffed)"
                        else:
                            row["deep_verified"] = True
                            row["verification_method"] = vm
                            row["note"] += f" | {compared} field(s) compared"
                    except ValueError:
                        pass
                results[opkey] = row
            else:
                results[opkey] = {"status_conformant": None, "schema_valid": sv, "note": f"H={hs}"}
    return results


def write_results(results):
    conformant = sum(1 for r in results.values() if r["status_conformant"] is True)
    schema_ok = sum(1 for r in results.values() if r["schema_valid"] is True)
    skipped = sum(1 for r in results.values() if "unresolved" in (r.get("note") or ""))
    deep_ok = sum(1 for r in results.values()
                  if r.get("deep_verified") is True
                  and r.get("verification_method") == verification.BODY_DIFF)
    deep_run = sum(1 for r in results.values() if "deep_verified" in r)
    empty = sum(1 for r in results.values()
                if r.get("verification_method") == verification.EMPTY_CORPUS
                and r.get("deep_verified") is True)
    out = {"generated_by": "suite/parity/sweep.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "rows": results}
    with open(os.path.join(ROOT, "suite/parity/sweep-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    print(f"wrote parity/sweep-results.json — {len(results)} ops, "
          f"{conformant} status-conformant, {schema_ok} schema-valid, {skipped} skipped (unfillable), "
          f"{deep_ok}/{deep_run} GET deep-diffed clean, {empty} both-empty (empty-corpus)")

# ---------------------------------------------------------------- self-check

def selfcheck():
    spec = load_spec()
    v = make_validator(spec)
    # nullable: a null in a nullable field passes; in a non-nullable field fails.
    assert v({"type": "object", "properties": {"x": {"type": "string", "nullable": True}}}, {"x": None})
    assert not v({"type": "object", "properties": {"x": {"type": "string"}}}, {"x": None})
    # $ref against the real spec resolves and validates a minimal instance.
    assert v({"$ref": "#/components/schemas/AuthenticationResult"}, {}) is True
    # path-param fill + skip detection.
    fx = {"itemId": "abc", "userId": "u1"}
    url, skip = build_url("/Items/{itemId}", fx)
    assert url == "/Items/abc" and skip is None, (url, skip)
    # `{pluginId}` used to be unresolvable, which skipped five plugin rows. It
    # is seeded now, so the guard is the positive one: with a seed the URL
    # builds, and WITHOUT one it still skips loudly rather than filling a blank.
    url, skip = build_url("/Plugins/{pluginId}", {**fx, "pluginId": "abc123"})
    assert url == "/Plugins/abc123" and skip is None, (url, skip)
    _, skip = build_url("/Plugins/{pluginId}", fx)
    assert skip and "pluginId" in skip
    # …and `{version}` is path-scoped to /Plugins/, so it cannot leak into
    # another route that happens to name a version.
    scoped = scoped_fixtures("/Plugins/{pluginId}/{version}/Image",
                             {**fx, "pluginId": "abc123", "_plugin_version": "1.0.0"})
    url, skip = build_url("/Plugins/{pluginId}/{version}/Image", scoped)
    assert url == "/Plugins/abc123/1.0.0/Image" and skip is None, (url, skip)
    # deep-diff field dedup collapses per-item [key] prefixes to one field each.
    df = dedup_fields({"missing": [{"path": "[Path=/a].Foo"}, {"path": "[Path=/b].Foo"}],
                       "extra": [], "mismatch": [{"path": "Bar"}]})
    assert df["missing"] == ["Foo"] and df["mismatch"] == ["Bar"], df
    # userId query injection.
    url = with_user_query("/Genres", {"parameters": [{"name": "userId", "in": "query"}]}, set(), "u1")
    assert url == "/Genres?userId=u1", url
    # required-query fill (+ static=true on ops that declare it); an unfillable required param
    # is left alone, an optional one is never filled.
    op = {"parameters": [{"name": "mediaSourceId", "in": "query", "required": True},
                         {"name": "static", "in": "query"},
                         {"name": "segmentLength", "in": "query"},
                         {"name": "deviceProfileId", "in": "query", "required": True}]}
    url = with_user_query("/Videos/abc/master.m3u8", op, {"itemId"}, "u1", {"mediaSourceId": "abc"})
    assert url == "/Videos/abc/master.m3u8?mediaSourceId=abc&static=true", url
    # the /Audio/* ops swap in the first track; no music library → unchanged.
    assert audio_fixtures({"itemId": "m", "_audio": "a"})["itemId"] == "a"
    assert audio_fixtures({"itemId": "m"})["itemId"] == "m"
    # `/Packages/{name}` names a PACKAGE, not a genre.
    assert package_fixtures("/Packages/{name}",
                            {"name": "Action", "_package": "Bookshelf"})["name"] == "Bookshelf"
    assert "name" not in package_fixtures("/Packages/{name}", {"name": "Action"})
    assert package_fixtures("/Genres/{name}", {"name": "Action"})["name"] == "Action"
    # A /Similar alias is probed with ITS OWN kind, not the shared movie id.
    assert similar_fixtures("/Albums/{itemId}/Similar",
                            {"itemId": "m", "_album": "al"})["itemId"] == "al"
    assert similar_fixtures("/Artists/{itemId}/Similar",
                            {"itemId": "m", "_artist": "ar"})["itemId"] == "ar"
    assert similar_fixtures("/Shows/{itemId}/Similar",
                            {"itemId": "m", "_series": "se"})["itemId"] == "se"
    # No such kind in the fixture, or a route that is not a /Similar alias: unchanged.
    assert similar_fixtures("/Albums/{itemId}/Similar", {"itemId": "m"})["itemId"] == "m"
    # The music-genre routes must be probed with a music genre, and nothing else
    # may be rewritten (a movie-genre seed on /MusicGenres/{genreName} is a
    # WRITE against Jellyfin — see `music_genre_fixtures`).
    mg = music_genre_fixtures("/MusicGenres/{genreName}",
                              {"genreName": "Action", "name": "Action", "_musicgenre": "Jazz"})
    assert mg["genreName"] == "Jazz" and mg["name"] == "Jazz", mg
    assert music_genre_fixtures("/Genres/{genreName}",
                                {"genreName": "Action", "_musicgenre": "Jazz"})["genreName"] == "Action"
    assert music_genre_fixtures("/MusicGenres/{genreName}",
                                {"genreName": "Action"})["genreName"] == "Action"
    assert similar_fixtures("/Movies/{itemId}/Similar",
                            {"itemId": "m", "_album": "al"})["itemId"] == "m"
    # status-class comparison is by hundreds bucket.
    assert (200 // 100) == (204 // 100) and (404 // 100) != (500 // 100)
    # A path-scoped param fills only inside its own path family. `channelId`
    # names a Live TV channel under /LiveTv/ and a plugin channel under
    # /Channels/; leaking the former into the latter would probe a real route
    # with an id from the wrong id space and call the result parity.
    assert build_url("/LiveTv/Channels/{channelId}", {"_livetv_channel": "CH"}) == ("/LiveTv/Channels/CH", None)
    assert build_url("/Channels/{channelId}/Items", {"_livetv_channel": "CH"})[0] is None
    # …and the same in the other direction: the plugin-channel seed fills only
    # under /Channels/, never a Live TV route.
    assert build_url("/Channels/{channelId}/Items", {"_plugin_channel": "PC"}) == ("/Channels/PC/Items", None)
    assert build_url("/Channels/{channelId}/Features", {"_plugin_channel": "PC"}) == ("/Channels/PC/Features", None)
    assert build_url("/LiveTv/Channels/{channelId}", {"_plugin_channel": "PC"})[0] is None
    # The deep-diff verdict must be derived from what was actually compared.
    from parity_diff import diff_stats as ds
    empty = {"Items": [], "TotalRecordCount": 0, "StartIndex": 0}
    assert ds(empty, empty)[0] == 0 and verification.read_method(empty, empty, ds(empty, empty)[2]) \
        == verification.EMPTY_CORPUS
    # a body that is 100% volatile (e.g. /GetUtcTime) compares nothing → untested
    utc = {"RequestReceptionTime": "a", "ResponseTransmissionTime": "b"}
    assert ds(utc, {"RequestReceptionTime": "c", "ResponseTransmissionTime": "d"})[2] == 0
    assert verification.read_method(utc, utc, 0) is None
    assert verification.read_method({"A": 1}, {"A": 1}, 1) == verification.BODY_DIFF
    print("ok: nullable, $ref, param-fill, path-scoped params, skip, query-inject, "
          "required-fill, status-class, verification-method derivation")


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    ferrofin = os.environ.get("FERROFIN_URL", "http://localhost:18096")
    jellyfin = os.environ.get("JELLYFIN_URL")   # optional oracle
    write_results(sweep(ferrofin, jellyfin))
    samples.flush()


if __name__ == "__main__":
    main()
