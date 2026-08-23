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


def provision_livetv(base, token):
    """The Live TV fixture: one M3U tuner host + one XMLTV listings provider (both read
    from the shared mount), then the guide refresh task, waited on until channels and
    programmes are listed. No-op when the fixture is off (LIVETV_M3U unset)."""
    m3u, xmltv = os.environ.get("LIVETV_M3U"), os.environ.get("LIVETV_XMLTV")
    if not m3u or not xmltv:
        return
    st, raw = http("POST", base + "/LiveTv/TunerHosts", token,
                   json.dumps({"Type": "m3u", "Url": m3u, "FriendlyName": "Parity tuner",
                               "ImportFavoritesOnly": False, "AllowHWTranscoding": False}))
    if st >= 300:
        raise SystemExit(f"{base}: add tuner host failed {st}: {raw[:200]!r}")
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
    last, stable = -1, 0
    for _ in range(480):
        n = count()
        stable = stable + 1 if (n == last and n > 0) else 0
        if stable >= 8:
            break
        last = n
        time.sleep(5)


CTX_USER = {}

def bring_up(base, target):
    # Idempotent: if already provisioned (e.g. an earlier producer in the same docker cycle),
    # just connect — don't re-run the wizard (fails once setup is complete) or re-add libraries.
    try:
        token, user = authenticate(base)
        CTX_USER[base] = user
        b = get_json(base, f"/Items?userId={user}&recursive=true&limit=0", token)
        if b and b.get("TotalRecordCount", 0) > 0:
            return token, user
    except SystemExit:
        pass
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
    any_item = movie or series or episode
    genres = get_json(base, f"/Genres?userId={user}&limit=1", token) or {}
    genre = (genres.get("Items") or [{}])[0].get("Name") or "Action"
    sessions = get_json(base, "/Sessions", token) or []
    session = sessions[0]["Id"] if sessions else None
    logs = get_json(base, "/System/Logs", token) or []
    log_name = logs[0]["Name"] if logs and logs[0].get("Name") else None

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
        "year": "2020", "container": "mp4", "segmentContainer": "ts", "format": "ts",
        "routeFormat": "ts", "width": "400", "maxWidth": "400", "maxHeight": "400",
        "percentPlayed": "0", "unplayedCount": "0", "tag": "x", "language": "eng",
        "routeStartPositionTicks": "0", "streamId": "0", "logName": log_name,
        # Not a path param: the first track, so the /Audio/* ops probe a real audio item
        # (see `audio_fixtures`).
        "_audio": audio, "_audio_src": source_id(audio),
    }
    return {k: v for k, v in fx.items() if v is not None}


def audio_fixtures(fixtures):
    """The fixtures with the item/source ids swapped for the first audio track — what the
    /Audio/* ops are about. Unchanged when the fixture has no music library."""
    audio = fixtures.get("_audio")
    if not audio:
        return fixtures
    return {**fixtures, "itemId": audio, "mediaSourceId": fixtures.get("_audio_src") or audio}


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


def build_url(path, fixtures):
    """Fill path params from one server's fixtures. Return (url, skip_reason_or_None)."""
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

from parity_diff import diff_counts  # noqa: E402


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
            if method not in ("get", "head"):   # writes are destructive/ordered → Layer-2 journeys
                results[opkey] = {"status_conformant": None, "schema_valid": None,
                                  "note": "write: deferred to Layer-2 journey"}
                continue
            fx_h = audio_fixtures(fixtures) if path.startswith("/Audio/") else fixtures
            fx_j = audio_fixtures(fixtures_j) if path.startswith("/Audio/") else fixtures_j
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
                        n, buckets = diff_counts(json.loads(jraw), json.loads(hraw))
                        row["deep_verified"] = n == 0
                        if n:
                            row["classification"] = "flagged: read diff vs Jellyfin (sweep single-item align)"
                            row["diffs"] = dedup_fields(buckets)
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
    deep_ok = sum(1 for r in results.values() if r.get("deep_verified") is True)
    deep_run = sum(1 for r in results.values() if "deep_verified" in r)
    out = {"generated_by": "suite/parity/sweep.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "rows": results}
    with open(os.path.join(ROOT, "suite/parity/sweep-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    print(f"wrote parity/sweep-results.json — {len(results)} ops, "
          f"{conformant} status-conformant, {schema_ok} schema-valid, {skipped} skipped (unfillable), "
          f"{deep_ok}/{deep_run} GET deep-diffed clean")

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
    _, skip = build_url("/Plugins/{pluginId}", fx)
    assert skip and "pluginId" in skip
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
    # status-class comparison is by hundreds bucket.
    assert (200 // 100) == (204 // 100) and (404 // 100) != (500 // 100)
    print("ok: nullable, $ref, param-fill, skip, query-inject, required-fill, status-class")


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    ferrofin = os.environ.get("FERROFIN_URL", "http://localhost:18096")
    jellyfin = os.environ.get("JELLYFIN_URL")   # optional oracle
    write_results(sweep(ferrofin, jellyfin))


if __name__ == "__main__":
    main()
