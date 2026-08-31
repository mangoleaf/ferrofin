"""JSON deep-diff for the Layer-2 read differential — a faithful port of
the retired k6 diff engine (benchmark/parity-diff.js), now the single diff implementation.

diff() walks two trees and buckets differences into mismatch/missing/extra.
Arrays are matched by a stable key (Path > Name > Id), not index, so two
independently-scanned servers whose ids differ still line up the SAME element.
`volatile` names keys that legitimately differ between instances (ids, dates,
per-server paths) and are skipped.
"""
import re

# Keys that legitimately differ between two independent instances/scans (from parity.js).
VOLATILE = re.compile("^(" + "|".join([
    "Id", "Key", "ItemId", "ImageTags", "ServerId", "ServerName", "Etag", "ETag", "PlaySessionId",
    "ImageBlurHashes",
    # Divergent-GUID family: item GUIDs derive per-scan, so every id that references another item
    # differs between independent servers (documented deferral — resolved by the shared-DB path).
    # id-correlation aligns the items themselves by Path; these cross-references still can't match.
    "ParentId", "SeriesId", "SeasonId", "AlbumId", "ParentLogoItemId", "ParentBackdropItemId",
    "ParentThumbItemId", "ParentArtItemId", "ParentPrimaryImageItemId", "PrimaryImageItemId",
    "DisplayPreferencesId",
    # BlurHash: valid, but not byte-identical to Jellyfin's Skia 128px downsample (documented deferral).
    "BlurHash",
    # Per-instance derived values: ImageTag is an md5(path+mtime) cache tag (mtime differs per scan);
    # OwnerId references another per-scan item id; the rest are per-session/per-request server values.
    "ImageTag", "OwnerId", "AccessToken", "DateLastActivity", "LastUserId",
    "RequestReceptionTime", "ResponseTransmissionTime",
    "DateCreated", "DateModified", "DateLastSaved", "DateLastMediaAdded", "DateLastRefreshed",
    "LastActivityDate", "LastLoginDate", "LastPlaybackCheckIn", "RemoteEndPoint", "UserId",
    "StartTimeUtc", "EndTimeUtc",
    "ProductName", "PackageName", "WebPath", "LocalAddress", "OperatingSystem",
    "OperatingSystemDisplayName", "SystemArchitecture", "EncoderLocation",
    "StartupWizardCompleted", "CanSelfRestart",
    "TranscodingTempPath", "LogPath", "InternalMetadataPath", "ItemsByNamePath", "CachePath",
    "ProgramDataPath",
    # `traceId` in an ASP.NET ProblemDetails error body: the W3C trace-context
    # id of the ONE request that produced it (`00-<32 hex>-<16 hex>-00`), minted
    # per request by the server that answered. Two independent instances cannot
    # produce the same one, and the same instance cannot produce it twice — the
    # textbook case this list is for. Scoped to that exact key; every other
    # field of a ProblemDetails (`type`, `title`, `status`, `errors`) is still
    # compared, which is what makes an error-body diff mean anything.
    "traceId",
]) + ")$")

ALIGN_KEYS = ("Path", "Name", "Id")


def _kind(x):
    if isinstance(x, list):
        return "array"
    if x is None:
        return "null"
    if isinstance(x, dict):
        return "object"
    return "leaf"


def _brief(x):
    import json
    s = json.dumps(x)
    return s[:80] + "…" if s and len(s) > 80 else s


def _align_key(e):
    if not isinstance(e, dict):
        return None
    for k in ALIGN_KEYS:
        if k in e and isinstance(e[k], (str, int, float)) and not isinstance(e[k], bool):
            return f"{k}={e[k]}"
    return None


def _keyed(arr):
    m = {}
    for e in arr:
        k = _align_key(e)
        if k is None or k in m:
            return None
        m[k] = e
    return m


def diff(j, h, path, out, volatile=VOLATILE, stats=None):
    """Walk both trees; append differences to out['mismatch'|'missing'|'extra'].

    `stats` (a dict) counts, in `stats["compared"]`, how many non-volatile LEAF
    comparisons actually executed. That count is not a nicety: a clean diff of an
    empty list, a bare `{}`, or a document whose every key is VOLATILE returns
    zero differences having compared nothing, and "nothing was compared" must
    never be recorded as "everything matched".
    """
    tj, th = _kind(j), _kind(h)
    if tj != th:
        if stats is not None:
            stats["compared"] += 1
        out["mismatch"].append({"path": path, "j": _brief(j), "h": _brief(h)})
        return
    if tj == "object":
        for k in set(j) | set(h):
            if volatile.match(k):
                continue
            p = f"{path}.{k}" if path else k
            if k not in h:
                out["missing"].append({"path": p, "j": _brief(j[k])})
            elif k not in j:
                out["extra"].append({"path": p, "h": _brief(h[k])})
            else:
                diff(j[k], h[k], p, out, volatile, stats)
    elif tj == "array":
        jk, hk = _keyed(j), _keyed(h)
        if jk is not None and hk is not None:
            for key in set(jk) | set(hk):
                p = f"{path}[{key}]"
                if key not in hk:
                    out["missing"].append({"path": f"{p} (whole item)"})
                elif key not in jk:
                    out["extra"].append({"path": f"{p} (whole item)"})
                else:
                    diff(jk[key], hk[key], p, out, volatile, stats)
        else:
            if len(j) != len(h):
                out["mismatch"].append({"path": f"{path}[]", "j": f"len {len(j)}", "h": f"len {len(h)}"})
            for i in range(min(len(j), len(h))):
                diff(j[i], h[i], f"{path}[{i}]", out, volatile, stats)
    else:
        if stats is not None:
            stats["compared"] += 1
        if j != h:
            out["mismatch"].append({"path": path, "j": _brief(j), "h": _brief(h)})


# `DtoService.GetChildCount` (Emby.Server.Implementations/Dto/DtoService.cs:649-656):
#
#     // Right now this is too slow to calculate for top level folders on a per-user basis
#     // Just return something so that apps that are expecting a value won't think the
#     // folders are empty
#     if (folder is ICollectionFolder || folder is UserView) { return Random.Shared.Next(1, 10); }
#
# so for exactly these client Types the field is a random draw in 1..9 on EVERY
# request and cannot be compared between two servers — five consecutive calls to
# one Jellyfin for the same folder returned 9, 2, 7, 2, 3. Ferrofin honours the
# same contract deterministically (`attach_child_count`, an id-derived 1..=9).
# `ICollectionFolder` also covers `BasePluginFolder` (BasePluginFolder.cs:12),
# hence the playlists folder.
#
# This is deliberately NOT a `VOLATILE` entry: `ChildCount` is a real, comparable
# value on a Series/Season/MusicAlbum/Playlist/Folder, and blanking it globally
# would stop the diff noticing a season that lost its episodes. The scrub is
# keyed on the sibling `Type`, so it can only ever reach the rows the C# randomizes.
RANDOM_CHILD_COUNT_TYPES = frozenset({"CollectionFolder", "UserView", "ManualPlaylistsFolder"})


def scrub_random_child_count(doc):
    """Drop `ChildCount` from the DTOs whose upstream value is `Random.Shared.Next(1, 10)`.

    Walks `doc` in place (and returns it) so the caller can wrap a parsed body
    directly. Apply to BOTH sides of a diff, never to one.
    """
    if isinstance(doc, dict):
        if doc.get("Type") in RANDOM_CHILD_COUNT_TYPES:
            doc.pop("ChildCount", None)
        for v in doc.values():
            scrub_random_child_count(v)
    elif isinstance(doc, list):
        for v in doc:
            scrub_random_child_count(v)
    return doc


def diff_counts(j, h):
    """Convenience: diff two docs, return (total_diffs, buckets_dict).

    Both sides go through [`scrub_random_child_count`] first — see its comment
    for why that one field is not diffable and why it is not in `VOLATILE`.
    """
    n, out, _ = diff_stats(j, h)
    return n, out


def diff_stats(j, h):
    """`diff_counts` plus the number of non-volatile leaf comparisons performed.

    A caller deciding HOW a row was verified needs the third value: `n == 0` with
    `compared == 0` is "the probe compared nothing", which is untested, while
    `n == 0` with `compared > 0` is a real agreement.
    """
    out = {"mismatch": [], "missing": [], "extra": []}
    stats = {"compared": 0}
    diff(scrub_random_child_count(j), scrub_random_child_count(h), "", out, VOLATILE, stats)
    n = len(out["mismatch"]) + len(out["missing"]) + len(out["extra"])
    return n, out, stats["compared"]
