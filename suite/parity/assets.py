#!/usr/bin/env python3
"""Layer-3 binary/asset differential: image, font, and CSS endpoints.

Binary responses can't be JSON-diffed, and byte-identity is the wrong bar — Ferrofin's
Rust `image` crate and Jellyfin's SkiaSharp re-encode the same source to different bytes.
So this layer diffs the *derived properties* a client actually depends on:

  - HTTP status class (200 vs 404 vs 400 — both servers agree),
  - Content-Type family (image / css / font / octet-stream),
  - for images, the DECODED format + dimensions (a JPEG of WxH), which also proves the
    resize/format transforms are honored identically (maxWidth=100 -> width<=100 on both),

plus round-trip EFFECTS for the uploads/deletes (POST an image -> GET it back decodes ->
DELETE -> GET 404), exactly like the write journeys.

Emits parity/asset-results.json; gen-ledger.py ingests it as the authority for the binary
ops (superseding their "not-testable-this-way: binary" classification).

Run via sweep.sh, or directly against provisioned servers:
  FERROFIN_URL=... JELLYFIN_URL=... parity/assets.py
Offline self-check:
  parity/assets.py --check
"""
import base64
import json
import os
import sys
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sweep import http, get_json, bring_up, ROOT   # noqa: E402

# A 1x1 PNG — the smallest valid upload payload; decodes to (png, 1, 1).
ONE_PX_PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)
DIM_TOLERANCE = 1   # px: allow a rounding difference between the two resamplers


# ------------------------------------------------------------- image header parser
def image_info(data):
    """(format, width, height) from raw bytes via header parsing (stdlib only).
    Returns (None, None, None) for a non-image / undecodable body."""
    if not data or len(data) < 24:
        return (None, None, None)
    if data[:8] == b"\x89PNG\r\n\x1a\n":
        return ("png", int.from_bytes(data[16:20], "big"), int.from_bytes(data[20:24], "big"))
    if data[:6] in (b"GIF87a", b"GIF89a"):
        return ("gif", int.from_bytes(data[6:8], "little"), int.from_bytes(data[8:10], "little"))
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        h = data[12:16]
        if h == b"VP8 ":
            return ("webp", int.from_bytes(data[26:28], "little") & 0x3FFF,
                    int.from_bytes(data[28:30], "little") & 0x3FFF)
        if h == b"VP8L":
            b = data[21:25]
            w = ((b[0] | (b[1] << 8)) & 0x3FFF) + 1
            ht = (((b[1] >> 6) | (b[2] << 2) | ((b[3] & 0x0F) << 10)) & 0x3FFF) + 1
            return ("webp", w, ht)
        if h == b"VP8X":
            return ("webp", int.from_bytes(data[24:27], "little") + 1,
                    int.from_bytes(data[27:30], "little") + 1)
        return ("webp", None, None)
    if data[:2] == b"\xff\xd8":  # JPEG: walk the markers to the SOF frame
        i = 2
        n = len(data)
        while i + 9 < n:
            if data[i] != 0xFF:
                i += 1
                continue
            marker = data[i + 1]
            if 0xC0 <= marker <= 0xCF and marker not in (0xC4, 0xC8, 0xCC):
                return ("jpeg", int.from_bytes(data[i + 7:i + 9], "big"),
                        int.from_bytes(data[i + 5:i + 7], "big"))
            i += 2 + int.from_bytes(data[i + 2:i + 4], "big")
        return ("jpeg", None, None)
    return (None, None, None)


def ct_family(ct):
    ct = (ct or "").lower().split(";")[0].strip()
    if ct.startswith("image/"):
        return "image"
    if ct in ("text/css",):
        return "css"
    if ct.startswith("font/") or ct in ("application/x-font-ttf", "application/font-woff",
                                        "application/font-sfnt", "application/octet-stream"):
        return "font-or-binary"
    return ct or "none"


# ------------------------------------------------------------- HTTP with raw bytes
def raw_headers(method, base, path, token, extra=None):
    """Returns (status, {lowercased header: value}, body_bytes). `extra` adds request headers."""
    import urllib.request
    import urllib.error
    hdr = {"Authorization": f'MediaBrowser Token="{token}", '
           'Client="parity", Device="parity", DeviceId="parity", Version="1.0"'}
    hdr.update(extra or {})
    req = urllib.request.Request(base + path, method=method, headers=hdr)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, {k.lower(): v for k, v in r.headers.items()}, r.read()
    except urllib.error.HTTPError as e:
        return e.code, {k.lower(): v for k, v in e.headers.items()}, e.read()
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        return 0, {}, b""


def raw(method, base, path, token):
    """Returns (status, content_type, body_bytes)."""
    st, h, body = raw_headers(method, base, path, token)
    return st, h.get("content-type", ""), body


def post_bytes(base, path, token, body, content_type):
    import urllib.request
    import urllib.error
    hdr = {"Content-Type": content_type,
           "Authorization": f'MediaBrowser Token="{token}", '
           'Client="parity", Device="parity", DeviceId="parity", Version="1.0"'}
    req = urllib.request.Request(base + path, data=body, method="POST", headers=hdr)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        return 0


# ------------------------------------------------------------- context resolution
def resolve(base, token, user_id):
    """Per-server context: an item that has a Primary image (+ its tag), and the shared
    by-name values (names identical across servers via the same NFO)."""
    b = get_json(base, f"/Items?userId={user_id}&recursive=true&includeItemTypes=Movie"
                       f"&fields=Path&limit=60&sortBy=SortName", token) or {}
    item, tag = "", ""
    for it in b.get("Items", []):
        tags = it.get("ImageTags") or {}
        if tags.get("Primary"):
            item, tag = it["Id"], tags["Primary"]
            break
    if not item and b.get("Items"):
        item = b["Items"][0]["Id"]                 # fall back to any item (image likely absent)
    # The file probes diff sha256, so they need the SAME file on both servers: the first movie
    # by SortName (same title → same Path), independent of which items carry images.
    file_item = b["Items"][0]["Id"] if b.get("Items") else ""

    def first_name(path):
        items = (get_json(base, f"{path}?userId={user_id}&limit=1", token) or {}).get("Items") or []
        return urllib.parse.quote(items[0]["Name"]) if items and items[0].get("Name") else "Nobody"

    return {
        "user": user_id, "item": item, "tag": tag or "x", "file_item": file_item,
        "genre": first_name("/Genres"), "studio": first_name("/Studios"),
        "person": first_name("/Persons"), "artist": "Nobody", "musicgenre": "Nobody",
    }


# ------------------------------------------------------------- read (property) probes
def read_signatures(base, token, c):
    """{op_key: signature} where signature is a comparable tuple derived from the response.
    For a 200 image: (status_class, ct_family, format, w, h). Otherwise: (status_class, '', ...)."""
    it, tag, u = c["item"], c["tag"], c["user"]
    reqs = [
        ("GET /Items/{itemId}/Images/{imageType}", "GET", f"/Items/{it}/Images/Primary"),
        ("GET /Items/{itemId}/Images/{imageType}/{imageIndex}", "GET", f"/Items/{it}/Images/Primary/0"),
        ("GET /Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}",
         "GET", f"/Items/{it}/Images/Primary/0/{tag}/jpg/100/100/0/0"),
        ("GET /Genres/{name}/Images/{imageType}", "GET", f"/Genres/{c['genre']}/Images/Primary"),
        ("GET /Genres/{name}/Images/{imageType}/{imageIndex}", "GET", f"/Genres/{c['genre']}/Images/Primary/0"),
        ("GET /Studios/{name}/Images/{imageType}", "GET", f"/Studios/{c['studio']}/Images/Primary"),
        ("GET /Studios/{name}/Images/{imageType}/{imageIndex}", "GET", f"/Studios/{c['studio']}/Images/Primary/0"),
        ("GET /Persons/{name}/Images/{imageType}", "GET", f"/Persons/{c['person']}/Images/Primary"),
        ("GET /Persons/{name}/Images/{imageType}/{imageIndex}", "GET", f"/Persons/{c['person']}/Images/Primary/0"),
        ("GET /Artists/{name}/Images/{imageType}/{imageIndex}", "GET", f"/Artists/{c['artist']}/Images/Primary/0"),
        ("GET /MusicGenres/{name}/Images/{imageType}", "GET", f"/MusicGenres/{c['musicgenre']}/Images/Primary"),
        ("GET /MusicGenres/{name}/Images/{imageType}/{imageIndex}", "GET", f"/MusicGenres/{c['musicgenre']}/Images/Primary/0"),
        ("GET /UserImage", "GET", f"/UserImage?userId={u}"),
        ("GET /Branding/Css", "GET", "/Branding/Css"),
        ("GET /Branding/Css.css", "GET", "/Branding/Css.css"),
        ("GET /FallbackFont/Fonts/{name}", "GET", "/FallbackFont/Fonts/nonexistent.woff2"),
    ]
    # HEAD mirrors of the image GETs (no body → signature is status + ct family only).
    heads = [
        ("HEAD /Items/{itemId}/Images/{imageType}", f"/Items/{it}/Images/Primary"),
        ("HEAD /Items/{itemId}/Images/{imageType}/{imageIndex}", f"/Items/{it}/Images/Primary/0"),
        ("HEAD /Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}",
         f"/Items/{it}/Images/Primary/0/{tag}/jpg/100/100/0/0"),
        ("HEAD /Genres/{name}/Images/{imageType}", f"/Genres/{c['genre']}/Images/Primary"),
        ("HEAD /Genres/{name}/Images/{imageType}/{imageIndex}", f"/Genres/{c['genre']}/Images/Primary/0"),
        ("HEAD /Studios/{name}/Images/{imageType}", f"/Studios/{c['studio']}/Images/Primary"),
        ("HEAD /Studios/{name}/Images/{imageType}/{imageIndex}", f"/Studios/{c['studio']}/Images/Primary/0"),
        ("HEAD /Persons/{name}/Images/{imageType}", f"/Persons/{c['person']}/Images/Primary"),
        ("HEAD /Persons/{name}/Images/{imageType}/{imageIndex}", f"/Persons/{c['person']}/Images/Primary/0"),
        ("HEAD /Artists/{name}/Images/{imageType}/{imageIndex}", f"/Artists/{c['artist']}/Images/Primary/0"),
        ("HEAD /MusicGenres/{name}/Images/{imageType}", f"/MusicGenres/{c['musicgenre']}/Images/Primary"),
        ("HEAD /MusicGenres/{name}/Images/{imageType}/{imageIndex}", f"/MusicGenres/{c['musicgenre']}/Images/Primary/0"),
        ("HEAD /UserImage", f"/UserImage?userId={u}"),
    ]
    out = {}
    for op, method, path in reqs:
        st, ct, body = raw(method, base, path, token)
        if st == 200 and ct_family(ct) == "image":
            out[op] = (2, "image", *image_info(body))
        elif st == 200:
            out[op] = (2, ct_family(ct), bool(body))   # css/font: family + non-empty
        else:
            out[op] = (st // 100, "")                   # non-200: status class parity
    for op, path in heads:
        st, ct, _ = raw("HEAD", base, path, token)
        out[op] = (st // 100, ct_family(ct) if st == 200 else "")
    # File-family ops. Both servers serve the SAME hardlinked fixture file, so the bar is the
    # file's sha256 plus the headers a download client depends on (type, ranges, disposition).
    fi = c.get("file_item")
    if fi:
        out["GET /Items/{itemId}/Download"] = file_sig(base, f"/Items/{fi}/Download", token)
        out["GET /Items/{itemId}/File"] = file_sig(base, f"/Items/{fi}/File", token, ranged=True)
    # BitrateTest is opaque bytes whose contract is "at least `size` of them": Jellyfin's
    # ArrayPool.Rent(1000) hands back a 1024-byte buffer and it ships the whole thing, so an
    # exact-length bar would flag the oracle's own over-delivery, not a Ferrofin gap.
    st, h, body = raw_headers("GET", base, "/Playback/BitrateTest?size=1000", token)
    out["GET /Playback/BitrateTest"] = ((2, ct_family(h.get("content-type")), len(body) >= 1000)
                                        if st == 200 else (st // 100, ""))
    # A log file's contents differ per instance by nature: type + non-empty is the contract.
    logs = get_json(base, "/System/Logs", token) or []
    name = logs[0]["Name"] if logs and logs[0].get("Name") else "missing.log"
    st, h, body = raw_headers("GET", base, "/System/Logs/Log?name=" + urllib.parse.quote(name), token)
    out["GET /System/Logs/Log"] = ((2, (h.get("content-type") or "").split(";")[0].strip(), bool(body))
                                   if st == 200 else (st // 100, ""))
    return out


def file_sig(base, path, token, ranged=False):
    """Signature of a served file: (2, 'file', sha256, content-type, accept-ranges,
    content-disposition) — plus the 206 status, Content-Range and sha256 of a 100-byte Range
    request when `ranged`. Non-200: (status_class, '')."""
    import hashlib
    st, h, body = raw_headers("GET", base, path, token)
    if st != 200:
        return (st // 100, "")
    sig = (2, "file", hashlib.sha256(body).hexdigest(), (h.get("content-type") or "").lower(),
           (h.get("accept-ranges") or "").lower(), h.get("content-disposition") or "")
    if ranged:
        rs, rh, rbody = raw_headers("GET", base, path, token, {"Range": "bytes=0-99"})
        sig += (rs, rh.get("content-range") or "", hashlib.sha256(rbody).hexdigest())
    return sig


def sig_match(h, j):
    """Two signatures agree — with a small pixel tolerance on image dimensions."""
    if h[:2] != j[:2]:
        return False
    if len(h) == 5 and len(j) == 5 and h[1] == "image":   # (2,'image',fmt,w,h)
        if h[2] != j[2]:
            return False
        for a, b in ((h[3], j[3]), (h[4], j[4])):
            if a is None or b is None:
                if a is not b:
                    return False
            elif abs(a - b) > DIM_TOLERANCE:
                return False
        return True
    return h == j


# ------------------------------------------------------------- write (effect) probes
def write_effects(base, token, c):
    """{op_key: bool} — upload/delete round-trips run per server, combined across servers by main()."""
    r = {}
    it, u = c["item"], c["user"]
    if it:
        # Item image upload → read-back decodes → delete → gone. Use Backdrop so Primary is untouched.
        st = post_bytes(base, f"/Items/{it}/Images/Backdrop", token,
                        base64.b64encode(ONE_PX_PNG), "image/png")
        back = raw("GET", base, f"/Items/{it}/Images/Backdrop", token)
        r["POST /Items/{itemId}/Images/{imageType}"] = st < 300 and image_info(back[2])[0] == "png"
        d = raw("DELETE", base, f"/Items/{it}/Images/Backdrop", token)[0]
        gone = raw("GET", base, f"/Items/{it}/Images/Backdrop", token)[0]
        r["DELETE /Items/{itemId}/Images/{imageType}"] = d < 300 and gone >= 400

        # Indexed variants: two backdrops (indices 0,1), reorder 1->0, delete by index.
        post_bytes(base, f"/Items/{it}/Images/Backdrop", token, base64.b64encode(ONE_PX_PNG), "image/png")
        st = post_bytes(base, f"/Items/{it}/Images/Backdrop", token,
                        base64.b64encode(ONE_PX_PNG), "image/png")
        r["POST /Items/{itemId}/Images/{imageType}/{imageIndex}"] = st < 300
        st = post_bytes(base, f"/Items/{it}/Images/Backdrop/1/Index?newIndex=0", token, b"", "application/json")
        r["POST /Items/{itemId}/Images/{imageType}/{imageIndex}/Index"] = st < 300
        d = raw("DELETE", base, f"/Items/{it}/Images/Backdrop/0", token)[0]
        r["DELETE /Items/{itemId}/Images/{imageType}/{imageIndex}"] = d < 300
        raw("DELETE", base, f"/Items/{it}/Images/Backdrop", token)   # cleanup any remaining

    # User image upload → read-back decodes → delete → gone.
    st = post_bytes(base, f"/UserImage?userId={u}", token, base64.b64encode(ONE_PX_PNG), "image/png")
    ub = raw("GET", base, f"/UserImage?userId={u}", token)
    r["POST /UserImage"] = st < 300 and image_info(ub[2])[0] == "png"
    d = raw("DELETE", base, f"/UserImage?userId={u}", token)[0]
    gone = raw("GET", base, f"/UserImage?userId={u}", token)[0]
    r["DELETE /UserImage"] = d < 300 and gone >= 400

    # Branding splashscreen: enable it (Ferrofin serves the GET only when enabled), upload,
    # read it back as an image, delete, then restore the disabled default.
    http("POST", f"{base}/System/Configuration/Branding", token,
         json.dumps({"SplashscreenEnabled": True, "LoginDisclaimer": "", "CustomCss": ""}))
    st = post_bytes(base, "/Branding/Splashscreen", token, base64.b64encode(ONE_PX_PNG), "image/png")
    sb = raw("GET", base, "/Branding/Splashscreen", token)
    r["POST /Branding/Splashscreen"] = st < 300
    r["GET /Branding/Splashscreen"] = sb[0] < 300 and ct_family(sb[1]) == "image"
    d = raw("DELETE", base, "/Branding/Splashscreen", token)[0]
    r["DELETE /Branding/Splashscreen"] = d < 300
    http("POST", f"{base}/System/Configuration/Branding", token,
         json.dumps({"SplashscreenEnabled": False, "LoginDisclaimer": "", "CustomCss": ""}))
    return r


# ------------------------------------------------------------- run
def run(ferrofin_url, jellyfin_url):
    ht, hu = bring_up(ferrofin_url, "ferrofin")
    jt, ju = bring_up(jellyfin_url, "jellyfin")
    hc, jc = resolve(ferrofin_url, ht, hu), resolve(jellyfin_url, jt, ju)

    rows = {}
    hsig, jsig = read_signatures(ferrofin_url, ht, hc), read_signatures(jellyfin_url, jt, jc)
    for op in sorted(hsig):
        h, j = hsig[op], jsig.get(op)
        ok = j is not None and sig_match(h, j)
        rows[op] = {"deep_verified": bool(ok),
                    "classification": "" if ok else "flagged: asset property diff vs Jellyfin (verify)",
                    "note": f"H={h} J={j}"}

    hw, jw = write_effects(ferrofin_url, ht, hc), write_effects(jellyfin_url, jt, jc)
    for op in sorted(hw):
        h_ok, j_ok = hw[op], jw.get(op)
        ok = bool(h_ok and j_ok)
        rows[op] = {"deep_verified": ok,
                    "classification": "" if ok else "flagged: asset write effect diff vs Jellyfin (verify)",
                    "note": f"H={h_ok} J={j_ok}"}
    return rows


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    ferrofin = os.environ.get("FERROFIN_URL", "http://localhost:18096")
    jellyfin = os.environ.get("JELLYFIN_URL", "http://localhost:18097")
    rows = run(ferrofin, jellyfin)
    out = {"generated_by": "suite/parity/assets.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "rows": rows}
    with open(os.path.join(ROOT, "suite/parity/asset-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"])
    print(f"wrote parity/asset-results.json — {len(rows)} asset ops, {ok} deep-verified")


def selfcheck():
    # image_info decodes the known payloads.
    assert image_info(ONE_PX_PNG) == ("png", 1, 1), image_info(ONE_PX_PNG)
    assert image_info(b"not an image") == (None, None, None)
    # a real JPEG header (SOI + APP0 + SOF0 3x5) decodes to (jpeg, 5, 3).
    jpeg = (b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00"
            b"\xff\xc0\x00\x11\x08\x00\x03\x00\x05\x03\x01\x22\x00\x02\x11\x01\x03\x11\x01")
    assert image_info(jpeg) == ("jpeg", 5, 3), image_info(jpeg)
    # sig_match: dimension tolerance + family/format strictness.
    assert sig_match((2, "image", "jpeg", 100, 75), (2, "image", "jpeg", 100, 76))
    assert not sig_match((2, "image", "jpeg", 100, 75), (2, "image", "png", 100, 75))
    assert not sig_match((2, "image", "jpeg", 100, 75), (2, "image", "jpeg", 100, 90))
    assert sig_match((4, ""), (4, ""))          # both 404 = parity
    assert not sig_match((2, "image", "jpeg", 100, 75), (4, ""))
    # every op key is a canonical spec path.
    import glob
    spec = json.load(open(sorted(glob.glob(os.path.join(ROOT, "contracts/jellyfin-openapi-*.json")))[-1]))
    valid = {f"{m.upper()} {p}" for p, it in spec["paths"].items() for m in it
             if m in ("get", "post", "put", "delete", "head")}
    # Build the op-key list without a live server (empty context).
    c = {"user": "U", "item": "I", "tag": "T", "genre": "G", "studio": "S",
         "person": "P", "artist": "A", "musicgenre": "M"}
    keys = set(read_signatures.__wrapped__(c) if hasattr(read_signatures, "__wrapped__") else [])
    # read_signatures needs a server; instead scan its literal op keys + the write op keys.
    import inspect
    declared = set()
    for fn in (read_signatures, write_effects):
        for line in inspect.getsource(fn).splitlines():
            if '("' in line and ('GET ' in line or 'HEAD ' in line or 'POST ' in line or 'DELETE ' in line):
                for part in line.split('("')[1:]:
                    key = part.split('"')[0]
                    if key.split(" ", 1)[0] in ("GET", "HEAD", "POST", "DELETE"):
                        declared.add(key)
    bad = sorted(k for k in declared if k not in valid)
    assert not bad, f"asset op-keys not in spec: {bad}"
    print(f"ok: image parser, sig_match, {len(declared)} asset op-keys valid")


if __name__ == "__main__":
    main()
