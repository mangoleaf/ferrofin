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
import verification  # noqa: E402  — the closed set of verification methods

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
    # by SortName (same title → same Path), independent of which items carry images. Its media
    # source id comes from PlaybackInfo, as a client obtains it (not the item id's spelling).
    file_item = b["Items"][0]["Id"] if b.get("Items") else ""
    file_src = file_item
    if file_item:
        info = get_json(base, f"/Items/{file_item}/PlaybackInfo?userId={user_id}", token) or {}
        sources = info.get("MediaSources") or []
        file_src = (sources[0].get("Id") or file_item) if sources else file_item

    def first_name(path):
        # by SortName, so both servers name the same entity
        items = (get_json(base, f"{path}?userId={user_id}&limit=1&sortBy=SortName", token)
                 or {}).get("Items") or []
        return urllib.parse.quote(items[0]["Name"]) if items and items[0].get("Name") else "Nobody"

    return {
        "user": user_id, "item": item, "tag": tag or "x", "file_item": file_item,
        "file_src": file_src,
        "genre": first_name("/Genres"), "studio": first_name("/Studios"),
        "person": first_name("/Persons"), "artist": first_name("/Artists"),
        "musicgenre": first_name("/MusicGenres"),
    }


# ------------------------------------------------------------- read (property) probes
#
# HOW these rows are verified, and why it is not a body diff.
#
# Ferrofin resizes/encodes with the Rust `image` crate and Jellyfin with Skia, so a
# transformed image's BYTES cannot match and byte-equality is not the contract. What
# IS the contract is the DECLARED properties: the status, the media type, the decoded
# container, and the pixel dimensions. Those are what these rows compare, so they are
# stamped `verification_method: "property"` — a real verification, but a weaker claim
# than the ledger's headline ("the response itself diffed clean"), and gen-ledger.py
# counts and renders it separately so the headline keeps meaning what it says.
#
# The exception is the file family (`/Download`, `/File`, `/Attachments/{index}`):
# both servers serve the SAME hardlinked fixture bytes, so those rows really are a
# byte-for-byte diff (sha256) and stay "body-diff".
SIG_PROPERTY = verification.PROPERTY
SIG_BODY_DIFF = verification.BODY_DIFF
SIG_EFFECT = verification.EFFECT
SIG_STATUS_CLASS = verification.STATUS_CLASS


# The pages every stock Jellyfin serves: its five IN-TREE provider plugins
# (`MediaBrowser.Providers/Plugins/{Tmdb,StudioImages,Omdb,MusicBrainz,AudioDb}/
# Plugin.cs`, each a `BasePlugin<PluginConfiguration>, IHasWebPages`). Plus one
# lowercase spelling (the C# match is `OrdinalIgnoreCase`) and two negative
# controls, so a server that answered 200 to any name could not pass.
CONFIG_PAGE_NAMES = ["TMDb", "tmdb", "Studio Images", "OMDb", "MusicBrainz", "AudioDB",
                     "NoSuchPlugin", ""]


def config_page_signature(base, token):
    """`(2|4, 'configpage', ((name, status, media-type, sha256), ...))`.

    The body is hashed, not sampled: these are embedded resources served
    verbatim on both servers, so byte-identity is the right bar. The
    Content-Type is compared as the bare media type — Jellyfin spells the
    charset `UTF-8` and Ferrofin `utf-8`, which is the same header value under
    RFC 9110 and not a divergence worth a row.
    """
    import hashlib
    sig = []
    for name in CONFIG_PAGE_NAMES:
        st, h, body = raw_headers(
            "GET", base, "/web/ConfigurationPage?name=" + urllib.parse.quote(name), token)
        media = (h.get("content-type") or "").split(";")[0].strip().lower()
        sig.append((name, st, media, hashlib.sha256(body).hexdigest() if st == 200 else ""))
    served = any(entry[1] == 200 for entry in sig)
    return (2 if served else 4, "configpage", tuple(sig))


def read_signatures(base, token, c):
    """`({op_key: signature}, {op_key: verification_method})`, where signature is a
    comparable tuple derived from the response. For a 200 image:
    (status_class, ct_family, format, w, h). Otherwise: (status_class, '', ...)."""
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
    out, methods = {}, {}
    for op, method, path in reqs:
        st, ct, body = raw(method, base, path, token)
        if st == 200 and ct_family(ct) == "image":
            out[op] = (2, "image", *image_info(body))
        elif st == 200:
            out[op] = (2, ct_family(ct), bool(body))   # css/font: family + non-empty
        else:
            out[op] = (st // 100, "")                   # non-200: status class parity
        methods[op] = SIG_PROPERTY
    # A HEAD has NO BODY, so nothing is ever served, decoded or measured here: the
    # signature is the status class plus a content-type family, which is verbatim
    # what the closed set defines as `status-class` ("at most also a content-type
    # family"). These 13 rows were stamped `property` and rendered "declared
    # properties agreed" while agreeing on nothing but a header.
    for op, path in heads:
        st, ct, _ = raw("HEAD", base, path, token)
        out[op] = (st // 100, ct_family(ct) if st == 200 else "")
        methods[op] = SIG_STATUS_CLASS
    # File-family ops. Both servers serve the SAME hardlinked fixture file, so the bar is the
    # file's sha256 plus the headers a download client depends on (type, ranges, disposition).
    fi, fs = c.get("file_item"), c.get("file_src")
    if fi:
        out["GET /Items/{itemId}/Download"] = file_sig(base, f"/Items/{fi}/Download", token)
        out["GET /Items/{itemId}/File"] = file_sig(base, f"/Items/{fi}/File", token, ranged=True)
        methods["GET /Items/{itemId}/Download"] = SIG_BODY_DIFF
        methods["GET /Items/{itemId}/File"] = SIG_BODY_DIFF
    # BitrateTest is opaque bytes whose contract is "at least `size` of them": Jellyfin's
    # ArrayPool.Rent(1000) hands back a 1024-byte buffer and it ships the whole thing, so an
    # exact-length bar would flag the oracle's own over-delivery, not a Ferrofin gap.
    st, h, body = raw_headers("GET", base, "/Playback/BitrateTest?size=1000", token)
    out["GET /Playback/BitrateTest"] = ((2, ct_family(h.get("content-type")), len(body) >= 1000)
                                        if st == 200 else (st // 100, ""))
    methods["GET /Playback/BitrateTest"] = SIG_PROPERTY
    # The fixture clip carries an attached font (stream index 3: video, audio, subtitle,
    # attachment), so both servers return the same bytes: sha256 + content type.
    if fi:
        out["GET /Videos/{videoId}/{mediaSourceId}/Attachments/{index}"] = file_sig(
            base, f"/Videos/{fi}/{fs}/Attachments/3", token)
        methods["GET /Videos/{videoId}/{mediaSourceId}/Attachments/{index}"] = SIG_BODY_DIFF
    # The dashboard's plugin configuration PAGES. These are embedded HTML
    # resources served verbatim (C# `GetManifestResourceStream` +
    # `MimeTypes.GetMimeType`), so unlike an image they CAN and must be compared
    # byte-for-byte — which is why they live in this layer rather than the JSON
    # read set.
    #
    # The breadth sweep can never see them: `name` is an OPTIONAL query
    # parameter, so `with_user_query` never fills it and the row only ever
    # compared the nameless refusal — trivially 404=404 on both while Jellyfin
    # served five pages and Ferrofin served none.
    #
    # `tmdb` is probed alongside `TMDb` because the C# match is
    # `StringComparison.OrdinalIgnoreCase`; `NoSuchPlugin` and the empty name are
    # the negative controls, so a server that answered 200 to everything could
    # not pass.
    out["GET /web/ConfigurationPage"] = config_page_signature(base, token)
    methods["GET /web/ConfigurationPage"] = SIG_BODY_DIFF
    # A log file's contents differ per instance by nature: type + non-empty is the contract.
    logs = get_json(base, "/System/Logs", token) or []
    name = logs[0]["Name"] if logs and logs[0].get("Name") else "missing.log"
    st, h, body = raw_headers("GET", base, "/System/Logs/Log?name=" + urllib.parse.quote(name), token)
    out["GET /System/Logs/Log"] = ((2, (h.get("content-type") or "").split(";")[0].strip(), bool(body))
                                   if st == 200 else (st // 100, ""))
    methods["GET /System/Logs/Log"] = SIG_PROPERTY
    return out, methods


# --------------------------------------------- content negotiation / route binding
#
# `read_signatures` only ever asks for a transform WITH an explicit `/jpg/` segment,
# so two whole classes of divergence were structurally invisible to this layer:
#
#   1. output-format negotiation — C# `ImageController.GetClientSupportedFormats`
#      derives the format list from the request's `Accept` header (and the `?Accept=`
#      query value); WebP is offered ONLY when advertised, so `Accept: */*` must come
#      back JPEG. Ferrofin used to hardcode `[Webp, Jpg, Png]` and hand WebP to every
#      client, forever.
#   2. `{format}` route binding — C# binds `[FromRoute, Required] ImageFormat`, so an
#      unbindable segment is a model-binding failure (400). Ferrofin used to fall back
#      to the default format list and answer 200 with an unrequested container.
#
# Both are compared as DECLARED properties (status class, Content-Type, decoded
# format) — never as bytes, which the two encoders cannot match.
NEGOTIATION_OPS = {
    "GET /Items/{itemId}/Images/{imageType}": "negotiation",
    "GET /Items/{itemId}/Images/{imageType}/{imageIndex}": "negotiation",
    "GET /Items/{itemId}/Images/{imageType}/{imageIndex}/{tag}/{format}"
    "/{maxWidth}/{maxHeight}/{percentPlayed}/{unplayedCount}": "binding",
}


def _decoded_sig(base, path, token, extra=None):
    """(status, content-type, decoded format) — the declared properties only.

    The status is EXACT, not the `// 100` class the byte-signature probes use: this
    matrix exists to police status codes, so a 400 that regressed to a 404 must fail.

    The Content-Type is compared for 200s only. Jellyfin's own error bodies are not
    self-consistent — a model-binding 400 is `application/json` ProblemDetails while an
    `ArgumentException` 400 out of `ExceptionMiddleware` is `text/plain` — and Ferrofin
    answers JSON for both. That is a server-wide error-envelope difference, not an
    image-contract one, so it is excluded here and recorded as a divergence instead."""
    st, h, body = raw_headers("GET", base, path, token, extra)
    if st != 200:
        return (st, "", "")
    return (200, (h.get("content-type") or "").split(";")[0].strip().lower(), image_info(body)[0])


def negotiation_signatures(base, token, c):
    """{op_key: tuple of sub-signatures} for the format-negotiation and route-binding
    behaviour of the item-image ops. Empty when the context has no item."""
    it, tag = c.get("item"), c.get("tag")
    if not it:
        return {}
    # Accept matrix on a transform that names NO format. `*/*` must NOT enable WebP
    # (`SupportsFormat(..., Webp, acceptAll: false)`); an explicit `image/webp` must.
    negotiate = tuple(
        _decoded_sig(base, f"/Items/{it}/Images/Primary?maxWidth=100", token, extra)
        for extra in (
            None,
            {"Accept": "*/*"},
            {"Accept": "image/jpeg"},
            {"Accept": "image/webp,image/apng,image/*,*/*;q=0.8"},
        )
    ) + (
        # the `?Accept=` query form of the same switch
        _decoded_sig(base, f"/Items/{it}/Images/Primary?maxWidth=100&Accept=webp", token),
    )
    # `?format=` is a SEPARATE binding arm from the `{format}` segment and diverges
    # from it, so it needs its own sub-probes: C# binds `[FromQuery] ImageFormat?`
    # (nullable => no `EnumTypeModelBinder` undefined-value check), so an unparseable
    # value falls back to negotiation (200) while an UNDEFINED ordinal binds and then
    # throws `InvalidEnumArgumentException` out of `GetMimeType` (400). Without these
    # the query arm was as invisible to this layer as the route arm used to be.
    negotiate += tuple(
        _decoded_sig(base, f"/Items/{it}/Images/Primary?maxWidth=100&format={v}", token)
        for v in ("bogus", "jpeg", "3.0", "-1", "6", "99",
                  "3", "png", "webp", "Jpg%2CPng", "Jpg%2CPng%2CWebp")
    )
    # Route-segment binding on the positional URL: a non-member `{format}` and a
    # non-numeric `{percentPlayed}` are 400s; a numeric enum ordinal binds (3 = Png).
    binding = tuple(
        _decoded_sig(base, f"/Items/{it}/Images/Primary/0/{tag}/{seg}", token)
        for seg in ("ts/100/100/0/0", "jpeg/100/100/0/0", "3/100/100/0/0",
                    "jpg/100/100/abc/0", "webp/100/100/0/0",
                    # the same undefined/loose-parse values as the query arm above —
                    # here they are 400s, which is exactly the divergence between arms.
                    "-1/100/100/0/0", "6/100/100/0/0",
                    "Jpg%2CPng/100/100/0/0", "Jpg%2CPng%2CWebp/100/100/0/0")
    )
    return {op: (negotiate if which == "negotiation" else binding)
            for op, which in NEGOTIATION_OPS.items()}


def primary_image_path(base, token, item):
    """The stored `Path` of an item's Primary image, as `/Items/{id}/Images` reports it.

    The property diff compares DERIVED properties of the served bytes, which is only
    meaningful when both servers are serving the same SOURCE file. When they are not,
    a dimension mismatch says nothing about the image endpoint — so the row must fail
    naming that, instead of implying a resize bug."""
    for info in (get_json(base, f"/Items/{item}/Images", token) or []):
        if info.get("ImageType") == "Primary":
            return info.get("Path") or ""
    return ""


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
        # The indexed upload must actually USE the indexed route. It used to be
        # recorded from a POST to the UNindexed URL, so the row reported the status
        # of a different endpoint — `SetItemImageByIndex` (a distinct C# action,
        # ImageController.cs:389) was never requested at all.
        post_bytes(base, f"/Items/{it}/Images/Backdrop", token, base64.b64encode(ONE_PX_PNG), "image/png")
        st = post_bytes(base, f"/Items/{it}/Images/Backdrop/0", token,
                        base64.b64encode(ONE_PX_PNG), "image/png")
        idx0 = raw("GET", base, f"/Items/{it}/Images/Backdrop/0", token)
        r["POST /Items/{itemId}/Images/{imageType}/{imageIndex}"] = (
            st < 300 and image_info(idx0[2])[0] == "png")
        post_bytes(base, f"/Items/{it}/Images/Backdrop", token, base64.b64encode(ONE_PX_PNG), "image/png")
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
    hsig, hmethod = read_signatures(ferrofin_url, ht, hc)
    jsig, _ = read_signatures(jellyfin_url, jt, jc)
    for op in sorted(hsig):
        h, j = hsig[op], jsig.get(op)
        ok = j is not None and sig_match(h, j)
        method = hmethod.get(op, SIG_PROPERTY)
        # HONESTY DOWNGRADE. A signature is `(status_class, kind_label, *evidence)`,
        # and a row where `evidence` is absent or entirely falsy established nothing
        # but the status class: the `(status // 100, "")` every helper falls back to
        # when the server served nothing, a headers-only HEAD, or a 200 whose body
        # was EMPTY (`/Branding/Css` at `(2, "css", False)`, the deliberately
        # nonexistent `/FallbackFont/Fonts/nonexistent.woff2` at `(2, "none", False)`).
        # None of those decoded an image or compared a property, so none may read as
        # "properties agreed" — a Ferrofin that 404'd every by-name image request
        # would otherwise pass 16 rows.
        if verification.bare_status_class(h) and verification.bare_status_class(j or ()):
            method = SIG_STATUS_CLASS
        rows[op] = {"deep_verified": bool(ok),
                    "classification": "" if ok else "flagged: asset property diff vs Jellyfin (verify)",
                    # HOW, not just whether — see the SIG_PROPERTY note above. Only the
                    # file family is a real byte diff; the image rows compare declared
                    # properties, which two different encoders CAN match and bytes cannot.
                    "verification_method": method,
                    "note": f"H={h} J={j}"
                            + ("" if method == SIG_BODY_DIFF or not ok
                               else " (status class only; nothing was served on either server)"
                               if method == SIG_STATUS_CLASS
                               else " (declared properties agreed; bytes not diffed)")}

    # Format negotiation + route binding, folded into the same rows.
    hneg = negotiation_signatures(ferrofin_url, ht, hc)
    jneg = negotiation_signatures(jellyfin_url, jt, jc)
    for op, h in hneg.items():
        j = jneg.get(op)
        row = rows.get(op)
        if row is None:
            continue
        if h != j:
            row["deep_verified"] = False
            row["classification"] = "flagged: asset property diff vs Jellyfin (verify)"
            row["note"] += f" | negotiation/binding H={h} J={j}"

    # Source-image guard: a dimension diff on the item-image ops only means the image
    # endpoint when both servers hold the SAME Primary file. Name it when they do not.
    hpath = primary_image_path(ferrofin_url, ht, hc["item"]) if hc.get("item") else ""
    jpath = primary_image_path(jellyfin_url, jt, jc["item"]) if jc.get("item") else ""
    if hpath != jpath:
        for op in NEGOTIATION_OPS:
            row = rows.get(op)
            if row is None:
                continue
            row["deep_verified"] = False
            row["classification"] = "flagged: asset property diff vs Jellyfin (verify)"
            row["note"] += f" | SOURCE IMAGE MISMATCH H={hpath!r} J={jpath!r}"

    hw, jw = write_effects(ferrofin_url, ht, hc), write_effects(jellyfin_url, jt, jc)
    for op in sorted(hw):
        h_ok, j_ok = hw[op], jw.get(op)
        ok = bool(h_ok and j_ok)
        # These are EFFECT verdicts (the write succeeded on both, and where a read-back
        # exists it decodes as the uploaded format). No body was diffed and the two
        # servers' responses were never compared with each other, so they carry
        # `effect` — distinct from `property`, where a property of a RESPONSE agreed.
        rows[op] = {"deep_verified": ok,
                    "classification": "" if ok else "flagged: asset write effect diff vs Jellyfin (verify)",
                    "verification_method": SIG_EFFECT,
                    "note": f"H={h_ok} J={j_ok}"
                            + (" (effect verdict; bodies not diffed)" if ok else "")}
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
    # read_signatures needs a server; instead scan its literal op keys + the write op keys
    # (both the `("GET …", …)` request tuples and the direct `out["GET …"] =` probes).
    import inspect
    import re
    declared = set()
    for fn in (read_signatures, write_effects):
        declared.update(re.findall(r'"((?:GET|HEAD|POST|DELETE) /[^"]+)"', inspect.getsource(fn)))
    # `negotiation_signatures` holds no op-key literals of its own — its keys are
    # NEGOTIATION_OPS, folded in here.
    declared.update(NEGOTIATION_OPS)
    bad = sorted(k for k in declared if k not in valid)
    assert not bad, f"asset op-keys not in spec: {bad}"
    # The negotiation/binding fold only ever tightens a row: it can turn a green row
    # red, never the other way round.
    assert set(NEGOTIATION_OPS).issubset(valid)
    # Honesty invariant: "body-diff" is the ledger's headline claim, so this layer may
    # only stamp it where the response BYTES were actually compared — the file family,
    # where both servers serve the same hardlinked fixture and the signature is a
    # sha256. Every other asset row compares declared properties and must say so.
    byte_exact = set(re.findall(r'methods\["((?:GET|HEAD|POST|DELETE) /[^"]+)"\] = SIG_BODY_DIFF',
                                inspect.getsource(read_signatures)))
    assert byte_exact == {
        "GET /Items/{itemId}/Download",
        "GET /Items/{itemId}/File",
        "GET /Videos/{videoId}/{mediaSourceId}/Attachments/{index}",
        # The plugin configuration pages are embedded resources served verbatim
        # on both servers (`GetManifestResourceStream`), and Ferrofin vendors
        # the same 10.11.8 files — so this row really does compare sha256s.
        "GET /web/ConfigurationPage",
    }, byte_exact
    assert (SIG_PROPERTY, SIG_BODY_DIFF) == ("property", "body-diff")
    assert {SIG_PROPERTY, SIG_BODY_DIFF, SIG_EFFECT, SIG_STATUS_CLASS} <= verification.VALID
    # The status-class downgrade: two bare `(class, "")` signatures matching is not a
    # property comparison, and must never be rendered as one.
    assert verification.bare_status_class((4, "")) and verification.bare_status_class((2, ""))
    assert not verification.bare_status_class((2, "image", "png", 600, 600))
    # An empty 200 body and a headers-only HEAD are status-class, not property.
    assert verification.bare_status_class((2, "css", False))
    assert verification.bare_status_class((2, "image"))
    # Every write row names the route it actually requested: the indexed upload must
    # POST to an INDEXED url, or the row reports a different endpoint's status.
    assert 'f"/Items/{it}/Images/Backdrop/0", token,' in inspect.getsource(write_effects)
    print(f"ok: image parser, sig_match, {len(declared)} asset op-keys valid, "
          f"{len(byte_exact)} byte-exact rows, status-class downgrade, "
          f"write rows stamped {SIG_EFFECT!r}")


if __name__ == "__main__":
    main()
